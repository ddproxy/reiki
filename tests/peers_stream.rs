//! `GET /v1/health/stream` and `reiki peers --watch`.
#![cfg(feature = "peers")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{router, App, Secret};

const ADMIN_TOKEN: &str = "admin-token-for-stream-tests";
const STREAM_TOKEN: &str = "stream-token-for-stream-tests";
const PEER_TOKEN: &str = "peer-token-for-stream-tests";
const CT5_TOKEN: &str = "ct-5-token-for-stream-tests";

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
}

async fn bind() -> (tokio::net::TcpListener, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

async fn serve_on(listener: tokio::net::TcpListener, router: Router) -> String {
    let addr = listener.local_addr().unwrap();
    tokio::spawn(reiki::serve::serve(
        listener,
        router,
        64,
        std::future::pending(),
    ));
    format!("http://{addr}")
}

async fn limited_upstream() -> String {
    let (listener, addr) = bind().await;
    let app = Router::new().fallback(|| async {
        let mut response = (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
        response
            .headers_mut()
            .insert("retry-after", "30".parse().unwrap());
        response
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct Rig {
    relay: String,
    peer: String,
    client: reqwest::Client,
    app: Arc<App>,
    text: String,
}

impl Rig {
    async fn new(peers_extra: &str) -> Self {
        let upstream = limited_upstream().await;
        let (peer_listener, peer_addr) = bind().await;
        let text = format!(
            "[upstream.zen]\nbase_url = \"{upstream}/v1\"\n\
             [keys.zen-a]\nupstream = \"zen\"\nfile = \"/unused/zen-a\"\n\
             [keys.zen-b]\nupstream = \"zen\"\nfile = \"/unused/zen-b\"\n\
             [streams.laptop]\ntoken_sha256 = \"{}\"\nchain = [\"zen-a\", \"zen-b\"]\n\
             [admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n\
             [peers]\nenabled = true\npublish = true\nlisten = \"{peer_addr}\"\n\
             peer_token_sha256 = \"{}\"\ninstance = \"streamer\"\n{peers_extra}",
            sha(STREAM_TOKEN),
            sha(ADMIN_TOKEN),
            sha(PEER_TOKEN),
        );
        let config = Config::parse(&text).unwrap();
        let keys: HashMap<String, Secret> = ["zen-a", "zen-b"]
            .iter()
            .map(|name| ((*name).to_owned(), Secret::new(format!("upstream-{name}"))))
            .collect();
        let app = App::new(
            config,
            keys,
            Policy::default(),
            reiki::plugins::PluginChain::default(),
        );
        let relay = serve_on(bind().await.0, router(Arc::clone(&app))).await;
        let peer = serve_on(peer_listener, reiki::peers::router(Arc::clone(&app))).await;
        Self {
            relay,
            peer,
            client: reqwest::Client::new(),
            app,
            text,
        }
    }

    /// Reloads the rig's config with `edit` applied, from real key files.
    fn reload(&self, label: &str, edit: impl Fn(&str) -> String) {
        let dir =
            std::env::temp_dir().join(format!("reiki-peers-stream-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["zen-a", "zen-b"] {
            std::fs::write(dir.join(name), format!("upstream-{name}\n")).unwrap();
        }
        let path = dir.join("reiki.toml");
        let text = edit(&self.text).replace("/unused/", &format!("{}/", dir.display()));
        std::fs::write(&path, text).unwrap();
        let report = self.app.reload_from_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
        report.unwrap();
    }

    async fn open(&self, token: &str) -> reqwest::Response {
        self.client
            .get(format!("{}/v1/health/stream", self.peer))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn cool_both_keys(&self) {
        let status = self
            .client
            .post(format!("{}/zen/chat/completions", self.relay))
            .bearer_auth(STREAM_TOKEN)
            .header("x-opencode-session", "ses_stream")
            .body("{}")
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 503);
    }
}

/// Reads SSE items, as `(event, data)` with `("", "")` for a comment, from a live response.
struct Reader {
    response: reqwest::Response,
    buffer: String,
}

impl Reader {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            buffer: String::new(),
        }
    }

    async fn next(&mut self) -> (String, String) {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if block.starts_with(':') {
                    return (String::new(), String::new());
                }
                let mut event = String::new();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        event = v.to_owned();
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data = v.to_owned();
                    }
                }
                return (event, data);
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.response.chunk())
                .await
                .expect("an event within 10 s")
                .unwrap()
                .expect("the stream stays open");
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// True when the server ends the stream within 10 s, whatever it sent before.
    async fn ends(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, self.response.chunk()).await {
                Err(_) => return false,
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None) | Err(_)) => return true,
            }
        }
    }

    /// The next item that is not a heartbeat or a host event.
    async fn next_change(&mut self) -> (String, String) {
        loop {
            let item = self.next().await;
            if !item.0.is_empty() && item.0 != "host" {
                return item;
            }
        }
    }
}

#[tokio::test]
async fn a_stream_starts_with_a_snapshot_then_sends_key_changes() {
    let rig = Rig::new("").await;
    let response = rig.open(PEER_TOKEN).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["cache-control"], "no-store");
    let mut reader = Reader::new(response);
    let (event, data) = reader.next().await;
    assert_eq!(event, "snapshot");
    let doc: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(doc["instance"], "streamer");
    assert_eq!(doc["upstreams"][0]["healthy_keys"], 2);

    rig.cool_both_keys().await;
    let (event, data) = reader.next_change().await;
    assert_eq!(event, "key", "{data}");
    let change: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(change["upstream"]["name"], "zen");
    assert_eq!(change["upstream"]["cooling_keys"], 2);
    assert!(change["observed_at"].is_string());
    assert!(
        change["upstream"].get("keys").is_none(),
        "no labels by default"
    );
    for hidden in ["laptop", "zen-a", "upstream-zen", "ses_stream"] {
        assert!(!data.contains(hidden), "{hidden} in {data}");
    }
}

#[tokio::test]
async fn labels_on_the_stream_follow_expose_key_labels() {
    let rig = Rig::new("expose_key_labels = true\n").await;
    let mut reader = Reader::new(rig.open(PEER_TOKEN).await);
    let (_, data) = reader.next().await;
    assert!(data.contains("\"keys\":[{"), "{data}");
    rig.cool_both_keys().await;
    let (event, data) = reader.next_change().await;
    assert_eq!(event, "key");
    assert!(data.contains("\"name\":\"zen-a\""), "{data}");
}

#[tokio::test]
async fn host_and_session_events_come_only_with_host_stats() {
    let rig = Rig::new("expose_host_stats = true\nstream_interval_secs = 1\n").await;
    let mut reader = Reader::new(rig.open(PEER_TOKEN).await);
    let (event, data) = reader.next().await;
    assert_eq!(event, "snapshot");
    assert!(data.contains("\"host\":"), "{data}");
    let mut saw_host = false;
    for _ in 0..3 {
        let (event, data) = reader.next().await;
        if event == "host" {
            assert!(data.contains("\"cores\":"), "{data}");
            saw_host = true;
            break;
        }
    }
    assert!(saw_host, "a host event within the interval");
    rig.cool_both_keys().await;
    let mut events = Vec::new();
    while events.len() < 2 {
        let (event, data) = reader.next_change().await;
        events.push((event, data));
    }
    let sessions = events
        .iter()
        .find(|(event, _)| event == "sessions")
        .expect("a sessions event");
    assert!(
        sessions.1.contains("\"active_sessions\":1"),
        "{}",
        sessions.1
    );
    assert!(!sessions.1.contains("ses_stream"));
}

#[tokio::test]
async fn the_stream_needs_the_peer_token_and_a_get() {
    let rig = Rig::new("").await;
    for token in [ADMIN_TOKEN, STREAM_TOKEN, "wrong-token"] {
        assert_eq!(rig.open(token).await.status(), 401, "{token}");
    }
    let res = rig
        .client
        .get(format!("{}/v1/health/stream", rig.peer))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = rig
        .client
        .post(format!("{}/v1/health/stream", rig.peer))
        .bearer_auth(PEER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let res = rig
        .client
        .get(format!("{}/v1/health/stream/more", rig.peer))
        .bearer_auth(PEER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn subscriber_caps_refuse_extra_streams_until_one_closes() {
    let rig = Rig::new("max_subscribers = 2\nmax_subscribers_per_peer = 1\n").await;
    let first = rig.open(PEER_TOKEN).await;
    assert_eq!(first.status(), 200);
    let second = rig.open(PEER_TOKEN).await;
    assert_eq!(second.status(), 429, "one stream per address");
    assert!(second.headers().contains_key("retry-after"));
    drop(first);
    let mut status = 0;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let res = rig.open(PEER_TOKEN).await;
        status = res.status().as_u16();
        if status == 200 {
            break;
        }
    }
    assert_eq!(status, 200, "a closed stream frees its place");
}

fn ct5_token() -> String {
    format!(
        "[[peers.tokens]]\nname = \"ct-5\"\ntoken_sha256 = \"{}\"\n",
        sha(CT5_TOKEN)
    )
}

#[tokio::test]
async fn every_configured_peer_token_opens_a_stream() {
    let rig = Rig::new(&ct5_token()).await;
    for token in [PEER_TOKEN, CT5_TOKEN] {
        let mut reader = Reader::new(rig.open(token).await);
        assert_eq!(reader.next().await.0, "snapshot", "{token}");
    }
    for token in [ADMIN_TOKEN, STREAM_TOKEN, "wrong-token"] {
        assert_eq!(rig.open(token).await.status(), 401, "{token}");
    }
}

#[tokio::test]
async fn removing_a_token_on_reload_ends_its_streams_and_keeps_the_others() {
    let rig = Rig::new(&ct5_token()).await;
    let mut removed = Reader::new(rig.open(CT5_TOKEN).await);
    let mut kept = Reader::new(rig.open(PEER_TOKEN).await);
    assert_eq!(removed.next().await.0, "snapshot");
    assert_eq!(kept.next().await.0, "snapshot");
    rig.reload("remove", |text| text.replace(&ct5_token(), ""));
    assert!(removed.ends().await, "the removed token's stream ends");
    assert_eq!(rig.open(CT5_TOKEN).await.status(), 401);
    rig.cool_both_keys().await;
    let (event, _) = kept.next_change().await;
    assert_eq!(event, "key", "the remaining token's stream stays open");
}

#[tokio::test]
async fn a_token_added_on_reload_works_at_once() {
    let rig = Rig::new("").await;
    assert_eq!(rig.open(CT5_TOKEN).await.status(), 401);
    rig.reload("add", |text| format!("{text}{}", ct5_token()));
    assert_eq!(rig.open(CT5_TOKEN).await.status(), 200);
}

#[tokio::test]
async fn the_per_token_cap_counts_one_token_whatever_the_address() {
    let rig = Rig::new(&format!(
        "max_subscribers = 8\nmax_subscribers_per_peer = 8\nmax_subscribers_per_token = 1\n{}",
        ct5_token()
    ))
    .await;
    let first = rig.open(CT5_TOKEN).await;
    assert_eq!(first.status(), 200);
    let second = rig.open(CT5_TOKEN).await;
    assert_eq!(second.status(), 429, "one stream for this token");
    assert!(second.headers().contains_key("retry-after"));
    assert_eq!(
        rig.open(PEER_TOKEN).await.status(),
        200,
        "another token has its own place"
    );
    drop(first);
    let mut status = 0;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        status = rig.open(CT5_TOKEN).await.status().as_u16();
        if status == 200 {
            break;
        }
    }
    assert_eq!(status, 200, "a closed stream frees the token's place");
}

struct TokenFile(PathBuf);

impl TokenFile {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("reiki-peers-watch-{label}-{}", std::process::id()));
        std::fs::write(&path, PEER_TOKEN).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self(path)
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn reader_config(url: &str, token: &TokenFile) -> Config {
    Config::parse(&format!(
        "[peers]\nenabled = true\npoll_interval_secs = 1\n\
         [peers.ct]\nurl = \"{url}\"\ntoken_file = \"{}\"\n",
        token.0.display()
    ))
    .unwrap()
}

#[tokio::test]
async fn watch_prints_the_snapshot_then_each_change() {
    let rig = Rig::new("").await;
    let token = TokenFile::new("stream");
    let config = reader_config(&rig.peer, &token);
    let peers = config.peers.clone().unwrap();
    let watcher = tokio::spawn(async move {
        let mut seen = Vec::new();
        let code = reiki::peers::client::watch(&peers, false, Some(2), |text| {
            seen.push(text.to_owned());
        })
        .await;
        (code, seen)
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    rig.cool_both_keys().await;
    let (code, seen) = tokio::time::timeout(Duration::from_secs(15), watcher)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(code, 0);
    let row = |table: &str| {
        table
            .lines()
            .nth(1)
            .unwrap()
            .split_whitespace()
            .take(6)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(row(&seen[0]), ["ct", "streamer", "zen", "ok", "2", "0"]);
    assert_eq!(
        row(&seen[1]),
        ["ct", "streamer", "zen", "cooling", "0", "2"]
    );
}

#[tokio::test]
async fn watch_falls_back_to_polling_when_the_peer_cannot_stream() {
    // A peer that only answers plain JSON, as a fronting proxy without SSE might.
    let (listener, addr) = bind().await;
    let observed = reiki_like_now();
    let app = Router::new().fallback(move || {
        let observed = observed.clone();
        async move {
            let body = format!(
                "{{\"instance\":\"plain\",\"observed_at\":\"{observed}\",\"upstreams\":[\
                 {{\"name\":\"zen\",\"healthy_keys\":1,\"cooling_keys\":0,\"disabled_keys\":0}}]}}"
            );
            ([("content-type", "application/json")], body).into_response()
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let token = TokenFile::new("poll");
    let config = reader_config(&format!("http://{addr}"), &token);
    let peers = config.peers.clone().unwrap();
    let mut seen = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(15),
        reiki::peers::client::watch(&peers, true, Some(1), |text| seen.push(text.to_owned())),
    )
    .await
    .unwrap();
    assert_eq!(code, 0);
    let doc: serde_json::Value = serde_json::from_str(seen[0].trim()).unwrap();
    assert_eq!(doc["peers"][0]["state"], "fresh");
    assert_eq!(doc["peers"][0]["instance"], "plain");
}

/// The current UTC time in the publisher's `observed_at` format.
fn reiki_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rem = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}
