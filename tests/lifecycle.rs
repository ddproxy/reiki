//! The process stops the way a Linux service should: SIGTERM and SIGINT drain requests in flight,
//! flush the request log and the state file, and exit 0; a second signal exits at once; systemd
//! hears READY and STOPPING.
#![cfg(unix)]

use std::os::unix::net::UnixDatagram;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use axum::Router;

mod common;
use common::write_key;

const STREAM_TOKEN: &str = "stream-token-for-lifecycle";

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
}

fn two_ports() -> (u16, u16) {
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    (
        a.local_addr().unwrap().port(),
        b.local_addr().unwrap().port(),
    )
}

struct Running {
    child: Child,
    dir: std::path::PathBuf,
    client: u16,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Running {
    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Waits for the child to exit and returns how, or `None` after `within`.
    fn wait_exit(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn start(label: &str, upstream: &str, notify: Option<&std::path::Path>) -> Running {
    for attempt in 0..3 {
        let (client, admin) = two_ports();
        let dir = std::env::temp_dir().join(format!(
            "reiki-lifecycle-{label}-{attempt}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_key(dir.join("a"), "secret-key-aaaaaaaa\n");
        let text = format!(
            "listen = \"127.0.0.1:{client}\"\n\
             [upstream.zen]\nbase_url = \"{upstream}/v1\"\n\
             [keys.zen-a]\nupstream = \"zen\"\nfile = \"{dir}/a\"\n\
             [streams.client-a]\ntoken_sha256 = \"{hash}\"\nchain = [\"zen-a\"]\n\
             [admin]\nlisten = \"127.0.0.1:{admin}\"\ntoken_sha256 = \"{admin_hash}\"\n\
             [plugins.requestlog]\nfile = \"{dir}/requests.jsonl\"\n\
             [service]\nshutdown_timeout_secs = 10\n",
            dir = dir.display(),
            hash = sha(STREAM_TOKEN),
            admin_hash = sha("admin-token-for-lifecycle"),
        );
        let config = dir.join("reiki.toml");
        std::fs::write(&config, text).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_reiki"));
        command
            .arg("--config")
            .arg(&config)
            .env_remove("REIKI_LISTEN")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(path) = notify {
            command.env("NOTIFY_SOCKET", path);
        }
        let child = command.spawn().unwrap();
        let mut running = Running { child, dir, client };
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if running.child.try_wait().unwrap().is_some() {
                break; // lost a port race; try again with fresh ports
            }
            if std::net::TcpStream::connect(("127.0.0.1", client)).is_ok() {
                return running;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    panic!("reiki never started");
}

fn slow_upstream(delay: Duration) -> Router {
    Router::new().fallback(move || async move {
        tokio::time::sleep(delay).await;
        "done"
    })
}

fn post(client: u16) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{client}/zen/chat/completions"))
        .bearer_auth(STREAM_TOKEN)
        .body("{}")
}

#[tokio::test]
async fn sigterm_stops_an_idle_instance_promptly_with_status_zero() {
    let upstream = common::serve(slow_upstream(Duration::from_millis(1))).await;
    let mut running = start("idle", &upstream, None);
    running.signal("TERM");
    let status = running
        .wait_exit(Duration::from_secs(5))
        .expect("it exited");
    assert!(status.success(), "{status:?}");
}

#[tokio::test]
async fn sigint_is_a_clean_stop_too() {
    let upstream = common::serve(slow_upstream(Duration::from_millis(1))).await;
    let mut running = start("int", &upstream, None);
    running.signal("INT");
    let status = running
        .wait_exit(Duration::from_secs(5))
        .expect("it exited");
    assert!(status.success(), "{status:?}");
}

#[tokio::test]
async fn a_request_in_flight_finishes_after_sigterm_and_new_ones_are_refused() {
    let upstream = common::serve(slow_upstream(Duration::from_millis(1500))).await;
    let mut running = start("drain", &upstream, None);
    let client = running.client;
    let in_flight = tokio::spawn(async move { post(client).send().await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    running.signal("TERM");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", client)).is_err(),
        "the listener should be closed while the request drains"
    );
    let response = in_flight.await.unwrap().expect("the request completed");
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "done");
    let status = running
        .wait_exit(Duration::from_secs(5))
        .expect("it exited");
    assert!(status.success(), "{status:?}");
}

#[tokio::test]
async fn a_second_signal_exits_at_once_with_the_signal_status() {
    let upstream = common::serve(slow_upstream(Duration::from_secs(30))).await;
    let mut running = start("second", &upstream, None);
    let client = running.client;
    let _held = tokio::spawn(async move { post(client).send().await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    running.signal("TERM");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        running.child.try_wait().unwrap().is_none(),
        "the first signal drains, it does not exit while a request is open"
    );
    running.signal("TERM");
    let status = running
        .wait_exit(Duration::from_secs(3))
        .expect("it exited");
    assert_eq!(status.code(), Some(143), "{status:?}");
}

#[tokio::test]
async fn the_request_log_is_flushed_before_exit() {
    // A burst of big records, so the writer is likely still busy when the signal arrives.
    const REQUESTS: usize = 100;
    let upstream = common::serve(slow_upstream(Duration::from_millis(1))).await;
    let mut running = start("log", &upstream, None);
    let body = format!("{{\"prompt\":\"{}\"}}", "word ".repeat(8_000));
    let client = running.client;
    let sends: Vec<_> = (0..REQUESTS)
        .map(|_| {
            let body = body.clone();
            tokio::spawn(async move {
                reqwest::Client::new()
                    .post(format!("http://127.0.0.1:{client}/zen/chat/completions"))
                    .bearer_auth(STREAM_TOKEN)
                    .body(body)
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
            })
        })
        .collect();
    for send in sends {
        assert_eq!(send.await.unwrap().unwrap(), 200);
    }
    running.signal("TERM");
    let status = running
        .wait_exit(Duration::from_secs(15))
        .expect("it exited");
    assert!(status.success(), "{status:?}");
    let log = std::fs::read_to_string(running.dir.join("requests.jsonl")).unwrap_or_default();
    let records = log
        .lines()
        .filter(|l| l.contains("\"event\":\"request\""))
        .count();
    assert_eq!(
        records, REQUESTS,
        "queued request records were lost at exit"
    );
}

#[tokio::test]
async fn the_state_file_is_written_before_exit() {
    let failing = Router::new()
        .fallback(|| async { (axum::http::StatusCode::UNAUTHORIZED, "invalid api key") });
    let upstream = common::serve(failing).await;
    let mut running = start("state", &upstream, None);
    let response = post(running.client).send().await.unwrap();
    assert_ne!(response.status(), 200);
    running.signal("TERM");
    let status = running
        .wait_exit(Duration::from_secs(5))
        .expect("it exited");
    assert!(status.success(), "{status:?}");
    let state = reiki::state::path_for(&running.dir.join("reiki.toml"));
    assert!(
        std::fs::metadata(&state).is_ok_and(|m| m.len() > 0),
        "no state file at {}",
        state.display()
    );
}

#[tokio::test]
async fn systemd_hears_ready_and_then_stopping() {
    let upstream = common::serve(slow_upstream(Duration::from_millis(1))).await;
    let dir = std::env::temp_dir().join(format!("reiki-lifecycle-notify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notify.sock");
    let socket = UnixDatagram::bind(&path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut running = start("notify", &upstream, Some(&path));
    let mut buf = [0u8; 512];
    let n = socket.recv(&mut buf).unwrap();
    let ready = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(ready.starts_with("READY=1"), "{ready:?}");
    running.signal("TERM");
    let n = socket.recv(&mut buf).unwrap();
    let stopping = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(stopping.starts_with("STOPPING=1"), "{stopping:?}");
    let status = running
        .wait_exit(Duration::from_secs(5))
        .expect("it exited");
    assert!(status.success(), "{status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
