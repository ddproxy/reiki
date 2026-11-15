//! `reiki top --once` against an in-test health server: its output, exit status and config
//! errors. The live loop is not tested against a real terminal.
#![cfg(feature = "top")]

use std::path::{Path, PathBuf};

const DOC: &str = r#"{"instance":"mock","observed_at":"t","upstreams":[
    {"name":"zen","healthy_keys":2,"cooling_keys":0,"disabled_keys":0,
     "soonest_cooldown_seconds":null,"last_status_class":"2xx"}]}"#;

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("reiki-top-cli-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn health_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app =
        axum::Router::new().fallback(|| async { ([("content-type", "application/json")], DOC) });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn reiki_top(config: &Path) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("--config")
        .arg(config)
        .args(["top", "--once"])
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn once_prints_the_fleet_and_exits_by_whether_any_peer_answered() {
    let dir = Dir::new("once");
    let token = dir.write("token", "token-for-top-tests", 0o600);
    let live = health_server().await;
    let dead = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let entry = |name: &str, url: String| {
        format!(
            "[peers.{name}]\nurl = \"{url}\"\ntoken_file = \"{}\"\n",
            token.display()
        )
    };
    let config = dir.write(
        "live.toml",
        &format!("[peers]\nenabled = true\n{}", entry("live", live)),
        0o600,
    );
    let path = config.clone();
    let (code, out, err) = tokio::task::spawn_blocking(move || reiki_top(&path))
        .await
        .unwrap();
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("Fleet 1/1 fresh"), "{out}");
    assert!(!out.contains("Providers"), "matrix is the default: {out}");
    assert!(out.contains("zen 1/1"), "{out}");
    assert!(err.is_empty(), "{err}");

    let config = dir.write(
        "dead.toml",
        &format!(
            "[peers]\nenabled = true\n{}",
            entry("dead", format!("http://127.0.0.1:{dead}"))
        ),
        0o600,
    );
    let path = config.clone();
    let (code, out, err) = tokio::task::spawn_blocking(move || reiki_top(&path))
        .await
        .unwrap();
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("no data"), "{out}");
    assert!(err.contains("peer `dead`"), "{err}");

    let config = dir.write("none.toml", "", 0o600);
    let (code, _, err) = tokio::task::spawn_blocking(move || reiki_top(&config))
        .await
        .unwrap();
    assert_eq!(code, 2);
    assert!(err.contains("no [peers]"), "{err}");
}
