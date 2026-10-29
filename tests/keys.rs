//! `reiki keys` reads key health from an admin listener and checks key files offline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, router, App};

const ADMIN_TOKEN: &str = "admin-token";
const STREAM_TOKEN: &str = "stream-token";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

struct Rig {
    dir: PathBuf,
    upstream_status: Arc<AtomicU16>,
    admin: String,
    relay: String,
    client: reqwest::Client,
}

fn config_text(dir: &Path, mock_url: &str) -> String {
    format!(
        "[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"{}\"\n\
         [keys.b]\nupstream = \"zen\"\nfile = \"{}\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [admin]\nlisten = \"127.0.0.1:41098\"\ntoken_sha256 = \"{}\"\n",
        dir.join("key-a").display(),
        dir.join("key-b").display(),
        reiki::auth::sha256_hex(STREAM_TOKEN),
        reiki::auth::sha256_hex(ADMIN_TOKEN),
    )
}

impl Rig {
    async fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("reiki-keys-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("key-a"), "upstream-secret-a\n").unwrap();
        std::fs::write(dir.join("key-b"), "upstream-secret-b\n").unwrap();
        let upstream_status = Arc::new(AtomicU16::new(200));
        let status = Arc::clone(&upstream_status);
        let mock_url = serve(Router::new().fallback(move |_: Request| {
            let status = Arc::clone(&status);
            async move {
                let code = status.load(Ordering::SeqCst);
                let mut response = Response::new(Body::from("{\"error\":\"mock\"}"));
                *response.status_mut() = StatusCode::from_u16(code).unwrap();
                if code == 429 {
                    response
                        .headers_mut()
                        .insert("retry-after", HeaderValue::from_static("120"));
                }
                response
            }
        }))
        .await;
        let text = config_text(&dir, &mock_url);
        let config = Config::parse(&text).unwrap();
        let keys = load_keys(&config).unwrap();
        let plugins = PluginChain::build(&config.plugins, &reiki::plugins::builtin()).unwrap();
        let app = App::with_plugins(
            config,
            keys,
            Policy::default(),
            Duration::from_secs(5),
            plugins,
        );
        let config_path = dir.join("reiki.toml");
        std::fs::write(&config_path, &text).unwrap();
        app.set_config_path(config_path.clone());
        let relay = serve(router(Arc::clone(&app))).await;
        let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
        Self {
            dir,
            upstream_status,
            admin,
            relay,
            client: reqwest::Client::new(),
        }
    }

    fn set_status(&self, status: u16) {
        self.upstream_status.store(status, Ordering::SeqCst);
    }

    async fn send(&self) -> u16 {
        self.client
            .post(format!("{}/zen/chat", self.relay))
            .bearer_auth(STREAM_TOKEN)
            .body("{}")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    fn token_file(&self) -> PathBuf {
        let path = self.dir.join("admin.token");
        std::fs::write(&path, format!("{ADMIN_TOKEN}\n")).unwrap();
        path
    }
}

#[tokio::test]
async fn the_table_shows_state_cooldown_status_and_streams() {
    let rig = Rig::new().await;
    rig.set_status(429);
    assert_ne!(rig.send().await, 200, "the only key cooled");
    let (url, token) = (rig.admin.clone(), rig.token_file().display().to_string());
    let (code, out, err) =
        tokio::task::spawn_blocking(move || reiki_keys(&["--url", &url, "--token-file", &token]))
            .await
            .unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("KEY"), "{out}");
    let a = out.lines().find(|line| line.starts_with("a ")).unwrap();
    assert!(a.contains("zen"), "{a}");
    assert!(a.contains("cooling"), "{a}");
    assert!(a.contains("429"), "{a}");
    assert!(a.ends_with("s"), "key a is in stream s: {a}");
    let b = out.lines().find(|line| line.starts_with("b ")).unwrap();
    assert!(b.contains("ok"), "{b}");
    assert!(b.ends_with('-'), "key b is in no stream: {b}");
    assert!(!out.contains("upstream-secret"), "{out}");
}

#[tokio::test]
async fn the_json_output_reports_disabled_keys_and_their_streams() {
    let rig = Rig::new().await;
    rig.set_status(401);
    assert_ne!(rig.send().await, 200, "the only key is disabled");
    let (url, token) = (rig.admin.clone(), rig.token_file().display().to_string());
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        reiki_keys(&["--json", "--url", &url, "--token-file", &token])
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{err}");
    let body: serde_json::Value = serde_json::from_str(&out).unwrap();
    let keys = body["keys"].as_array().unwrap();
    let a = keys.iter().find(|key| key["name"] == "a").unwrap();
    assert_eq!(a["state"], "disabled");
    assert_eq!(a["last_status_code"], 401);
    assert_eq!(a["cooldown_seconds"], serde_json::Value::Null);
    assert_eq!(a["streams"], serde_json::json!(["s"]));
    let b = keys.iter().find(|key| key["name"] == "b").unwrap();
    assert_eq!(b["state"], "ok");
    assert_eq!(b["last_status_code"], serde_json::Value::Null);
    assert_eq!(b["streams"], serde_json::json!([]));
    assert!(!out.contains("upstream-secret"), "{out}");
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn reiki_keys(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("keys")
        .args(args)
        .env_remove("REIKI_ADMIN_TOKEN")
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(mode);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn check_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reiki-keys-check-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn check_reports_missing_empty_and_world_readable_files() {
    let dir = check_dir("problems");
    let good = dir.join("good");
    let empty = dir.join("empty");
    let world = dir.join("world");
    std::fs::write(&good, "secret-good\n").unwrap();
    std::fs::write(&empty, "\n").unwrap();
    std::fs::write(&world, "secret-world\n").unwrap();
    #[cfg(unix)]
    {
        set_mode(&good, 0o600);
        set_mode(&empty, 0o600);
        set_mode(&world, 0o644);
    }
    let config = dir.join("reiki.toml");
    std::fs::write(
        &config,
        format!(
            "[upstream.zen]\nbase_url = \"https://zen.test\"\n\
             [keys.good]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.missing]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.empty]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.world]\nupstream = \"zen\"\nfile = \"{}\"\n",
            good.display(),
            dir.join("missing").display(),
            empty.display(),
            world.display(),
        ),
    )
    .unwrap();
    let path = config.display().to_string();
    let (code, out, err) = reiki_keys(&["--check", "--config", &path]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("good: ok"), "{out}");
    assert!(
        out.contains("missing: error: ") && out.contains("does not exist"),
        "{out}"
    );
    assert!(
        out.contains("empty: error: ") && out.contains("is empty"),
        "{out}"
    );
    assert!(
        out.contains("world: warning: ") && out.contains("group or world readable"),
        "{out}"
    );
    assert!(!out.contains("secret-good") && !out.contains("secret-world"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_clean_check_exits_zero_and_unreadable_config_exits_two() {
    let dir = check_dir("clean");
    let good = dir.join("good");
    std::fs::write(&good, "secret-good\n").unwrap();
    #[cfg(unix)]
    set_mode(&good, 0o600);
    let config = dir.join("reiki.toml");
    std::fs::write(
        &config,
        format!(
            "[upstream.zen]\nbase_url = \"https://zen.test\"\n\
             [keys.good]\nupstream = \"zen\"\nfile = \"{}\"\n",
            good.display()
        ),
    )
    .unwrap();
    let path = config.display().to_string();
    let (code, out, err) = reiki_keys(&["--check", "--config", &path]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("good: ok"), "{out}");

    let missing = dir.join("not-there.toml").display().to_string();
    let (code, _, err) = reiki_keys(&["--check", "--config", &missing]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("cannot read"), "{err}");

    let (code, _, _) = reiki_keys(&["--check", "--url", "http://127.0.0.1:1"]);
    assert_eq!(code, 2, "offline check rejects a URL");
    let _ = std::fs::remove_dir_all(&dir);
}
