//! `reiki keys` reads key health from an admin listener and checks key files offline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{App, load_keys, router};

mod common;
use common::serve;
use common::write_key;

const ADMIN_TOKEN: &str = "admin-token";
const STREAM_TOKEN: &str = "stream-token";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

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
        write_key(dir.join("key-a"), "upstream-secret-a\n");
        write_key(dir.join("key-b"), "upstream-secret-b\n");
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

    async fn admin_get(&self, path: &str, token: bool) -> (u16, String) {
        let mut request = self.client.get(format!("{}{path}", self.admin));
        if token {
            request = request.bearer_auth(ADMIN_TOKEN);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
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
    assert!(out.starts_with("KEY"), "the key table must have a header");
    let a = out.lines().find(|line| line.starts_with("a ")).unwrap();
    assert!(a.contains("zen"), "{a}");
    assert!(a.contains("cooling"), "{a}");
    assert!(a.contains("429"), "{a}");
    assert!(a.ends_with("s"), "key a is in stream s: {a}");
    let b = out.lines().find(|line| line.starts_with("b ")).unwrap();
    assert!(b.contains("ok"), "{b}");
    assert!(b.ends_with('-'), "key b is in no stream: {b}");
    assert!(
        !out.contains("upstream-secret"),
        "the key report must not include key contents"
    );
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
    assert!(
        !out.contains("upstream-secret"),
        "the key report must not include key contents"
    );
}

#[tokio::test]
async fn the_json_output_carries_the_cooldown_class() {
    let rig = Rig::new().await;
    rig.set_status(429);
    assert_ne!(rig.send().await, 200, "the only key cooled");
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
    assert_eq!(a["state"], "cooling");
    assert_eq!(a["cooldown_class"], "rate_limited");
    let b = keys.iter().find(|key| key["name"] == "b").unwrap();
    assert!(b.get("cooldown_class").is_none(), "{b}");
}

#[tokio::test]
async fn the_failure_route_needs_the_admin_token_and_honours_limit_and_key() {
    let rig = Rig::new().await;
    rig.set_status(429);
    assert_ne!(rig.send().await, 200);
    assert_ne!(rig.send().await, 200);

    let (code, _) = rig.admin_get("/v1/failures", false).await;
    assert_eq!(code, 401, "no token");

    let (code, body) = rig.admin_get("/v1/failures", true).await;
    assert_eq!(code, 200, "the failures endpoint must answer 200");
    let all: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert!(!all.is_empty(), "the failure ring must have an entry");
    let (code, _bare) = rig.admin_get("/failures", true).await;
    assert_eq!(code, 404, "the bare route is not served");
    let first = &all[0];
    assert_eq!(first["key"], "a");
    assert_eq!(first["stream"], "s");
    assert_eq!(first["status"], 429);
    assert_eq!(first["class"], "rate_limited");
    assert_eq!(first["retry_after"], "120");
    assert_eq!(first["cooled"], true);
    assert!(
        first["time"].as_u64().is_some(),
        "the failure must have a time"
    );
    assert!(
        first["note"].as_str().unwrap().contains("mock"),
        "the failure note must be present"
    );

    let (_, body) = rig.admin_get("/v1/failures?limit=1", true).await;
    let one: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(one.len(), 1, "the limit must return one entry");

    let (_, body) = rig.admin_get("/v1/failures?key=b", true).await;
    let empty: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert!(empty.is_empty(), "the key filter must exclude the failure");

    for bad in ["?limit=0", "?limit=65", "?limit=x", "?key=%zz", "?other=1"] {
        let (code, _) = rig.admin_get(&format!("/v1/failures{bad}"), true).await;
        assert_eq!(code, 400, "{bad}");
    }
}

#[tokio::test]
async fn the_failures_table_and_json_read_the_same_ring() {
    let rig = Rig::new().await;
    rig.set_status(429);
    assert_ne!(rig.send().await, 200);
    let (url, token) = (rig.admin.clone(), rig.token_file().display().to_string());
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        reiki_failures(&["--url", &url, "--token-file", &token])
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{err}");
    let header = out.lines().next().unwrap();
    for column in ["TIME", "STREAM", "KEY", "STATUS", "CLASS", "COOLED", "NOTE"] {
        assert!(header.contains(column), "{header}");
    }
    let row = out.lines().nth(1).unwrap();
    assert!(
        row.contains("rate_limited"),
        "the failure class must be shown"
    );
    assert!(row.contains("429"), "the failure status must be shown");

    let (url, token) = (rig.admin.clone(), rig.token_file().display().to_string());
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        reiki_failures(&[
            "--json",
            "--limit",
            "1",
            "--url",
            &url,
            "--token-file",
            &token,
        ])
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{err}");
    let body: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(body.len(), 1, "the JSON limit must return one entry");

    let (url, token) = (rig.admin.clone(), rig.token_file().display().to_string());
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        reiki_failures(&["--url", &url, "--token-file", &token, "--key", "gone"])
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "no failures\n");
}

#[tokio::test]
async fn the_failures_table_is_cut_to_columns() {
    let rig = Rig::new().await;
    rig.set_status(429);
    assert_ne!(rig.send().await, 200);
    let (url, token) = (rig.admin.clone(), rig.token_file().display().to_string());
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        reiki_failures_width(&["--url", &url, "--token-file", &token], "30")
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{err}");
    for line in out.lines() {
        assert!(
            line.chars().count() <= 30,
            "the failure line must be clipped"
        );
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn reiki_keys(args: &[&str]) -> (i32, String, String) {
    reiki_command("keys", args, None)
}

fn reiki_failures(args: &[&str]) -> (i32, String, String) {
    reiki_command("failures", args, None)
}

fn reiki_failures_width(args: &[&str], columns: &str) -> (i32, String, String) {
    reiki_command("failures", args, Some(columns))
}

fn reiki_command(command: &str, args: &[&str], columns: Option<&str>) -> (i32, String, String) {
    let mut process = Command::new(env!("CARGO_BIN_EXE_reiki"));
    process
        .arg(command)
        .args(args)
        .env_remove("REIKI_ADMIN_TOKEN");
    if let Some(columns) = columns {
        process.env("COLUMNS", columns);
    }
    let out = process.output().unwrap();
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
    let (code, out, _err) = reiki_keys(&["--check", "--config", &path]);
    assert_eq!(code, 1, "the key check must fail");
    assert!(
        out.contains("good: ok"),
        "the check must report the good key"
    );
    assert!(
        out.contains("missing: error: ") && out.contains("does not exist"),
        "the missing key file must be reported"
    );
    assert!(
        out.contains("empty: error: ") && out.contains("is empty"),
        "the empty key file must be reported"
    );
    assert!(
        out.contains("world: warning: ") && out.contains("group or world read/write bits"),
        "the loose key file must be reported"
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
    let (code, out, _err) = reiki_keys(&["--check", "--config", &path]);
    assert_eq!(code, 0, "the clean key check must pass");
    assert!(
        out.contains("good: ok"),
        "the check must report the good key"
    );

    let missing = dir.join("not-there.toml").display().to_string();
    let (code, _, err) = reiki_keys(&["--check", "--config", &missing]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("cannot read"), "{err}");

    let (code, _, _) = reiki_keys(&["--check", "--url", "http://127.0.0.1:1"]);
    assert_eq!(code, 2, "offline check rejects a URL");
    let _ = std::fs::remove_dir_all(&dir);
}
