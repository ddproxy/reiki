use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{App, Clock, Secret, router};

mod common;
use common::serve;
use common::write_key;

#[derive(Clone, Debug)]
struct Call {
    path_and_query: String,
    authorization: String,
    user_agent: String,
    custom: String,
    content_length: String,
    body: Vec<u8>,
}

type PathScript = HashMap<(String, String), VecDeque<Reply>>;

enum Reply {
    Status(u16, &'static str, Option<(&'static str, &'static str)>),
    Headers(u16, &'static str, Vec<(&'static str, &'static str)>),
    Bytes(u16, Vec<u8>, Vec<(&'static str, &'static str)>),
    Slow(Vec<&'static str>, Duration),
    Delay(Duration),
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A valid gzip stream holding `data` in one stored (uncompressed) block.
fn gzip(data: &[u8]) -> Vec<u8> {
    let len = u16::try_from(data.len()).unwrap();
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1];
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&u32::from(len).to_le_bytes());
    out
}

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<Call>>,
    script: Mutex<HashMap<String, VecDeque<Reply>>>,
    path_script: Mutex<PathScript>,
}

impl Mock {
    fn script(&self, key: &str, replies: Vec<Reply>) {
        self.script
            .lock()
            .unwrap()
            .insert(key.to_owned(), replies.into());
    }
    fn script_path(&self, key: &str, path: &str, replies: Vec<Reply>) {
        self.path_script
            .lock()
            .unwrap()
            .insert((key.to_owned(), path.to_owned()), replies.into());
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    fn keys_called(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|c| c.authorization.trim_start_matches("Bearer ").to_owned())
            .collect()
    }
}

async fn mock_handler(State(mock): State<Arc<Mock>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap().to_vec();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let authorization = header("authorization");
    mock.calls.lock().unwrap().push(Call {
        path_and_query: parts
            .uri
            .path_and_query()
            .map(ToString::to_string)
            .unwrap_or_default(),
        authorization: authorization.clone(),
        user_agent: header("user-agent"),
        custom: header("x-opencode-session"),
        content_length: header("content-length"),
        body,
    });
    let key = authorization.trim_start_matches("Bearer ").to_owned();
    let path = parts.uri.path().to_owned();
    let scripted_for_path = mock
        .path_script
        .lock()
        .unwrap()
        .get_mut(&(key.clone(), path))
        .and_then(VecDeque::pop_front);
    let reply = scripted_for_path.or_else(|| {
        mock.script
            .lock()
            .unwrap()
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
    });
    match reply {
        Some(Reply::Status(code, text, extra)) => {
            let mut b = Response::builder().status(code);
            if let Some((k, v)) = extra {
                b = b.header(k, v);
            }
            b.body(Body::from(text)).unwrap()
        }
        Some(Reply::Headers(code, text, headers)) => {
            let mut b = Response::builder().status(code);
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b.body(Body::from(text)).unwrap()
        }
        Some(Reply::Bytes(code, bytes, headers)) => {
            let mut b = Response::builder().status(code);
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b.body(Body::from(bytes)).unwrap()
        }
        Some(Reply::Slow(chunks, gap)) => {
            let stream = futures_util::stream::unfold(
                (chunks.into_iter(), true),
                move |(mut it, first)| async move {
                    if !first {
                        tokio::time::sleep(gap).await;
                    }
                    it.next().map(|c| {
                        (
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(c)),
                            (it, false),
                        )
                    })
                },
            );
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Some(Reply::Delay(wait)) => {
            tokio::time::sleep(wait).await;
            Response::new(Body::from("{\"ok\":true}"))
        }
        None => Response::new(Body::from("{\"ok\":true}")),
    }
}

struct Harness {
    mock: Arc<Mock>,
    mock_url: String,
    relay: String,
    client: reqwest::Client,
    app: Arc<App>,
    clock: Arc<TestClock>,
    admin: Option<String>,
}

/// The clock injected into test apps: pinned at `new` or `set`, then advancing with real time.
struct TestClock {
    state: Mutex<(SystemTime, Instant)>,
}

impl TestClock {
    fn at(now: SystemTime) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((now, Instant::now())),
        })
    }

    fn set(&self, now: SystemTime) {
        *self.state.lock().unwrap() = (now, Instant::now());
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        let (base, started) = *self.state.lock().unwrap();
        base.checked_add(started.elapsed()).unwrap_or(base)
    }
}

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
}

fn config_for(
    mock_url: &str,
    keys: &[(&str, &str)],
    streams: &[(&str, &str, &[&str])],
) -> (Config, HashMap<String, Secret>) {
    config_with_limits(mock_url, keys, streams, &[])
}

fn config_with_limits(
    mock_url: &str,
    keys: &[(&str, &str)],
    streams: &[(&str, &str, &[&str])],
    limits: &[(&str, u32)],
) -> (Config, HashMap<String, Secret>) {
    let mut text = format!("[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n");
    let mut map = HashMap::new();
    for (name, secret) in keys {
        text.push_str(&format!(
            "[keys.{name}]\nupstream = \"zen\"\nfile = \"/unused/{name}\"\n"
        ));
        map.insert((*name).to_owned(), Secret::new(*secret));
    }
    for (name, token, chain) in streams {
        let chain = chain
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let limit = limits
            .iter()
            .find(|(limited, _)| limited == name)
            .map(|(_, limit)| format!("\nrate_limit_per_minute = {limit}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "[streams.{name}]\ntoken_sha256 = \"{}\"\nchain = [{chain}]{limit}\n",
            sha(token)
        ));
    }
    (Config::parse(&text).unwrap(), map)
}

/// Builds the same config as [`config_for`] plus an `[upstream.zen.limits]` table,
/// so the shaper shapes every key of the one upstream.
fn config_with_shaper(
    mock_url: &str,
    keys: &[(&str, &str)],
    streams: &[(&str, &str, &[&str])],
    limits: &str,
) -> (Config, HashMap<String, Secret>) {
    let mut text = format!("[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n{limits}\n");
    let mut map = HashMap::new();
    for (name, secret) in keys {
        text.push_str(&format!(
            "[keys.{name}]\nupstream = \"zen\"\nfile = \"{}/{name}\"\n",
            key_dir().display()
        ));
        map.insert((*name).to_owned(), Secret::new(*secret));
    }
    for (name, token, chain) in streams {
        let chain = chain
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "[streams.{name}]\ntoken_sha256 = \"{}\"\nchain = [{chain}]\n",
            sha(token)
        ));
    }
    text.push_str("[plugins.metrics]\n");
    text.push_str(&format!(
        "[admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n",
        sha("admin-secret")
    ));
    (Config::parse(&text).unwrap(), map)
}

fn config_with_quota_resets(
    mock_url: &str,
    keys: &[(&str, &str)],
    resets: &[(&str, &str)],
    streams: &[(&str, &str, &[&str])],
) -> (Config, HashMap<String, Secret>) {
    let mut text = format!("[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n");
    let mut map = HashMap::new();
    for (name, secret) in keys {
        let reset = resets
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| format!("\nquota_resets = \"{value}\""))
            .unwrap_or_default();
        text.push_str(&format!(
            "[keys.{name}]\nupstream = \"zen\"\nfile = \"/unused/{name}\"{reset}\n"
        ));
        map.insert((*name).to_owned(), Secret::new(*secret));
    }
    for (name, token, chain) in streams {
        let chain = chain
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "[streams.{name}]\ntoken_sha256 = \"{}\"\nchain = [{chain}]\n",
            sha(token)
        ));
    }
    text.push_str(&format!(
        "[admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n",
        sha("admin-secret")
    ));
    (Config::parse(&text).unwrap(), map)
}

async fn harness(streams: &[(&str, &str, &[&str])], policy: Policy) -> Harness {
    harness_with_timeout(streams, policy, Duration::from_secs(120)).await
}

async fn harness_with_timeout(
    streams: &[(&str, &str, &[&str])],
    policy: Policy,
    header_timeout: Duration,
) -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;

    let keys = [("a", "up-a"), ("b", "up-b"), ("c", "up-c"), ("d", "up-d")];
    let (config, keys) = config_for(&mock_url, &keys, streams);
    assemble(mock, mock_url, config, keys, policy, header_timeout).await
}

async fn harness_with_limits(
    streams: &[(&str, &str, &[&str])],
    limits: &[(&str, u32)],
    policy: Policy,
) -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;

    let keys = [("a", "up-a"), ("b", "up-b"), ("c", "up-c"), ("d", "up-d")];
    let (config, keys) = config_with_limits(&mock_url, &keys, streams, limits);
    assemble(
        mock,
        mock_url,
        config,
        keys,
        policy,
        Duration::from_secs(120),
    )
    .await
}

/// A harness whose single upstream has an outbound `limits` table, with the wall
/// clock pinned so a test can jump between requests.
async fn harness_with_shaper(
    streams: &[(&str, &str, &[&str])],
    limits: &str,
    wall: SystemTime,
) -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;

    let keys = [("a", "up-a"), ("b", "up-b"), ("c", "up-c"), ("d", "up-d")];
    let (config, keys) = config_with_shaper(&mock_url, &keys, streams, limits);
    let plugins =
        reiki::plugins::PluginChain::build(&config.plugins, &reiki::plugins::builtin()).unwrap();
    let clock = TestClock::at(wall);
    let app = App::with_plugins_and_clock(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(120),
        plugins,
        Arc::clone(&clock) as Arc<dyn Clock>,
    );
    let relay = serve(router(Arc::clone(&app))).await;
    let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
    Harness {
        mock,
        mock_url,
        relay,
        client: reqwest::Client::new(),
        app,
        clock,
        admin: Some(admin),
    }
}

async fn harness_with_admin(
    streams: &[(&str, &str, &[&str])],
    admin_token: &str,
    policy: Policy,
) -> Harness {
    harness_with_admin_opts(streams, admin_token, policy, true).await
}

/// Key files that exist, so saved key health is restored (state for an unreadable key file is
/// dropped). Their modified time is old, so state saved now is newer than they are.
fn key_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("reiki-proxy-keys-{}", std::process::id()));
        let dir = dir.join("unused");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a", "b", "c", "d"] {
            let path = dir.join(name);
            write_key(&path, format!("up-{name}\n"));
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
                .unwrap();
        }
        dir
    })
}

async fn harness_with_admin_opts(
    streams: &[(&str, &str, &[&str])],
    admin_token: &str,
    policy: Policy,
    metrics: bool,
) -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;

    let keys = [("a", "up-a"), ("b", "up-b"), ("c", "up-c"), ("d", "up-d")];
    let mut text = format!("[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n");
    let mut key_map = HashMap::new();
    for (name, secret) in keys {
        text.push_str(&format!(
            "[keys.{name}]\nupstream = \"zen\"\nfile = \"{}/{name}\"\n",
            key_dir().display()
        ));
        key_map.insert(name.to_owned(), Secret::new(secret));
    }
    for (name, token, chain) in streams {
        let chain = chain
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "[streams.{name}]\ntoken_sha256 = \"{}\"\nchain = [{chain}]\n",
            sha(token)
        ));
    }
    text.push_str(&format!(
        "[admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n",
        sha(admin_token)
    ));
    if metrics {
        text.push_str("[plugins.metrics]\n");
    }
    let config = Config::parse(&text).unwrap();
    let plugins =
        reiki::plugins::PluginChain::build(&config.plugins, &reiki::plugins::builtin()).unwrap();
    let clock = TestClock::at(SystemTime::now());
    let app = App::with_plugins_and_clock(
        config,
        key_map,
        policy,
        Duration::from_secs(120),
        plugins,
        Arc::clone(&clock) as Arc<dyn Clock>,
    );
    let relay = serve(router(Arc::clone(&app))).await;
    let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
    Harness {
        mock,
        mock_url,
        relay,
        client: reqwest::Client::new(),
        app,
        clock,
        admin: Some(admin),
    }
}

async fn harness_with_quota_resets(
    streams: &[(&str, &str, &[&str])],
    resets: &[(&str, &str)],
    now: SystemTime,
) -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;

    let keys = [("a", "up-a"), ("b", "up-b"), ("c", "up-c"), ("d", "up-d")];
    let (config, keys) = config_with_quota_resets(&mock_url, &keys, resets, streams);
    let clock = TestClock::at(now);
    let app = App::with_header_timeout_and_clock(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(120),
        Arc::clone(&clock) as Arc<dyn Clock>,
    );
    let relay = serve(router(Arc::clone(&app))).await;
    let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
    Harness {
        mock,
        mock_url,
        relay,
        client: reqwest::Client::new(),
        app,
        clock,
        admin: Some(admin),
    }
}

async fn harness_with_upstream(
    streams: &[(&str, &str, &[&str])],
    upstream_extra: &str,
    policy: Policy,
) -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;

    let keys = [("a", "up-a"), ("b", "up-b"), ("c", "up-c"), ("d", "up-d")];
    let mut text = format!("[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n{upstream_extra}");
    let mut key_map = HashMap::new();
    for (name, secret) in keys {
        text.push_str(&format!(
            "[keys.{name}]\nupstream = \"zen\"\nfile = \"/unused/{name}\"\n"
        ));
        key_map.insert(name.to_owned(), Secret::new(secret));
    }
    for (name, token, chain) in streams {
        let chain = chain
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "[streams.{name}]\ntoken_sha256 = \"{}\"\nchain = [{chain}]\n",
            sha(token)
        ));
    }
    let config = Config::parse(&text).unwrap();
    assemble(
        mock,
        mock_url,
        config,
        key_map,
        policy,
        Duration::from_secs(120),
    )
    .await
}

async fn assemble(
    mock: Arc<Mock>,
    mock_url: String,
    config: Config,
    keys: HashMap<String, Secret>,
    policy: Policy,
    header_timeout: Duration,
) -> Harness {
    let clock = TestClock::at(SystemTime::now());
    let app = App::with_header_timeout_and_clock(
        config,
        keys,
        policy,
        header_timeout,
        Arc::clone(&clock) as Arc<dyn Clock>,
    );
    let relay = serve(router(Arc::clone(&app))).await;
    Harness {
        mock,
        mock_url,
        relay,
        client: reqwest::Client::new(),
        app,
        clock,
        admin: None,
    }
}

impl Harness {
    fn post(&self, token: &str, path: &str, body: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.relay))
            .bearer_auth(token)
            .header("user-agent", "client/1.0")
            .header("x-opencode-session", "ses_123")
            .body(body.to_owned())
    }

    fn reload(&self, keys: &[(&str, &str)], streams: &[(&str, &str, &[&str])]) {
        let (config, keys) = config_for(&self.mock_url, keys, streams);
        self.app.reload(config, keys);
    }

    fn reload_shaped(
        &self,
        keys: &[(&str, &str)],
        streams: &[(&str, &str, &[&str])],
        limits: &str,
    ) {
        let (config, keys) = config_with_shaper(&self.mock_url, keys, streams, limits);
        self.app.reload(config, keys);
    }
}

fn one_stream() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    vec![("client-a", "tok-a", &["a", "b"][..])]
}

#[tokio::test]
async fn forwards_path_query_body_and_headers_and_swaps_only_the_credential() {
    let h = harness(&one_stream(), Policy::default()).await;
    let res = h
        .post("tok-a", "/zen/chat/completions?stream=false", "{\"m\":1}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "{\"ok\":true}");
    let calls = h.mock.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path_and_query, "/v1/chat/completions?stream=false");
    assert_eq!(calls[0].authorization, "Bearer up-a");
    assert_eq!(calls[0].user_agent, "client/1.0");
    assert_eq!(calls[0].custom, "ses_123");
    assert_eq!(calls[0].body, b"{\"m\":1}");
}

#[tokio::test]
async fn accepts_a_query_key_token_and_removes_it_before_forwarding() {
    let h = harness(&one_stream(), Policy::default()).await;
    let res = h
        .client
        .post(format!(
            "{}/zen/chat/completions?key=tok-a&alt=sse",
            h.relay
        ))
        .header("user-agent", "gemini-client")
        .body("{\"m\":1}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let calls = h.mock.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].authorization, "Bearer up-a");
    assert_eq!(calls[0].path_and_query, "/v1/chat/completions?alt=sse");
    assert!(!calls[0].path_and_query.contains("key"));
}

#[tokio::test]
async fn fails_over_on_429_replaying_the_same_body_and_remembers_the_cooldown() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "30")),
        )],
    );
    let res = h
        .post("tok-a", "/zen/chat/completions", "payload-1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
    assert!(h.mock.calls().iter().all(|c| c.body == b"payload-1"));

    let res = h
        .post("tok-a", "/zen/chat/completions", "payload-2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-b"]);
}

const REQUEST_TOO_LARGE: &str = r#"{"error":{"message":"This request requires more credits, or fewer max_tokens. You requested up to 256000 tokens, but can only afford 199460."}}"#;

async fn failure_ring(h: &Harness) -> serde_json::Value {
    let admin = h.admin.as_ref().unwrap();
    let text = h
        .client
        .get(format!("{admin}/v1/failures"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn a_request_size_402_fails_over_without_cooling_the_key() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(402, REQUEST_TOO_LARGE, None)]);

    let res = h
        .post("tok-a", "/zen/chat/completions", "{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "{\"ok\":true}");
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
    assert_eq!(admin_key_state(&h, "a").await, "available");

    let failures = failure_ring(&h).await;
    let rows = failures.as_array().unwrap();
    assert_eq!(rows.len(), 1, "the failure ring must have one entry");
    assert_eq!(rows[0]["class"], "request_error");
    assert_eq!(rows[0]["cooled"], false);
}

#[tokio::test]
async fn a_request_size_402_from_every_key_is_returned_unchanged() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(402, REQUEST_TOO_LARGE, None)]);
    h.mock
        .script("up-b", vec![Reply::Status(402, REQUEST_TOO_LARGE, None)]);

    let res = h
        .post("tok-a", "/zen/chat/completions", "{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 402);
    assert_eq!(res.text().await.unwrap(), REQUEST_TOO_LARGE);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
    assert_eq!(admin_key_state(&h, "a").await, "available");
    assert_eq!(admin_key_state(&h, "b").await, "available");

    let failures = failure_ring(&h).await;
    let rows = failures.as_array().unwrap();
    assert_eq!(rows.len(), 2, "the failure ring must have two entries");
    assert!(rows.iter().all(|row| row["class"] == "request_error"));
    assert!(rows.iter().all(|row| row["cooled"] == false));
}

#[tokio::test]
async fn an_upstream_rewrites_the_model_and_drops_params_before_every_replay() {
    let h = harness_with_upstream(
        &one_stream(),
        "model_aliases = { \"gpt-4o\" = \"zen-gpt-4o\" }\ndrop_params = [\"stream_options\"]\n",
        Policy::default(),
    )
    .await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "30")),
        )],
    );

    let body = r#"{"model":"gpt-4o","stream_options":{"include_usage":true},"messages":[{"role":"user","content":"caf\u00e9 世界"}]}"#;
    let expected =
        r#"{"model":"zen-gpt-4o","messages":[{"role":"user","content":"caf\u00e9 世界"}]}"#;

    let res = h
        .post("tok-a", "/zen/chat/completions", body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);

    let calls = h.mock.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(
            std::str::from_utf8(&call.body).unwrap(),
            expected,
            "the upstream saw an unrewritten body"
        );
        assert_eq!(
            call.content_length,
            expected.len().to_string(),
            "content-length did not match the rewritten body"
        );
    }
    assert_eq!(calls[0].body, calls[1].body, "the replay body differed");
}

#[tokio::test]
async fn an_upstream_without_model_settings_forwards_the_body_unchanged() {
    let h = harness_with_upstream(&one_stream(), "", Policy::default()).await;
    let body = r#"{"model":"gpt-4o","stream_options":true}"#;
    let res = h
        .post("tok-a", "/zen/chat/completions", body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.calls()[0].body, body.as_bytes());
}

#[tokio::test]
async fn quota_and_disabled_access_both_move_to_the_next_key() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Free usage exceeded, subscribe to Go",
            None,
        )],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let h2 = harness(&one_stream(), Policy::default()).await;
    h2.mock.script(
        "up-a",
        vec![Reply::Status(
            500,
            "Upstream request failed: Model access is disabled",
            None,
        )],
    );
    assert_eq!(
        h2.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h2.mock.keys_called(), ["up-a", "up-b"]);
}

#[tokio::test]
async fn a_request_body_that_stalls_is_timed_out() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h =
        harness_with_timeout(&one_stream(), Policy::default(), Duration::from_millis(300)).await;
    let addr = h.relay.trim_start_matches("http://").to_owned();
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            b"POST /zen/x HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-a\r\nContent-Length: 100\r\n\r\nab",
        )
        .await
        .unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_buf(&mut reply))
        .await
        .expect("no answer to a stalled body")
        .unwrap();
    assert!(String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 408"));
    assert!(h.mock.keys_called().is_empty());
}

#[tokio::test]
async fn a_slow_body_client_on_the_client_listener_gets_a_bounded_408() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Same App as production, but on the real client listener (reiki::serve), rather
    // than axum::serve, and with the body deadline shortened so the test is quick.
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;
    let (config, key_map) = config_for(&mock_url, &[("a", "up-a"), ("b", "up-b")], &one_stream());
    let app = App::with_header_timeout(
        config,
        key_map,
        Policy::default(),
        Duration::from_millis(300),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(reiki::serve::serve_with(
        listener,
        router(app),
        8,
        Duration::from_secs(5), // headers are complete; only the body stalls
        std::future::pending(),
    ));

    // A fixed Content-Length with only part of the body, and a chunked body with
    // no terminating chunk, both leave a client stalled after a complete head.
    let requests = [
        "POST /zen/x HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-a\r\nContent-Length: 100\r\n\r\nab",
        "POST /zen/x HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-a\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n",
    ];
    for request in requests {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(3), async {
            let mut reply = Vec::new();
            while !String::from_utf8_lossy(&reply).contains("body_timeout") {
                if stream.read_buf(&mut reply).await.unwrap() == 0 {
                    break;
                }
            }
            reply
        })
        .await
        .expect("no answer to a stalled body");
        let reply = String::from_utf8_lossy(&reply);
        assert!(reply.starts_with("HTTP/1.1 408"), "reply was {reply}");
        assert!(reply.contains("body_timeout"), "reply was {reply}");
    }
    assert!(mock.keys_called().is_empty());
}

#[tokio::test]
async fn without_hold_unauthorized_the_first_refusing_key_is_disabled_at_once() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    h.mock
        .script("up-b", vec![Reply::Status(401, "invalid api key", None)]);
    h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    h.mock.script(
        "up-b",
        vec![Reply::Status(429, "slow down", Some(("retry-after", "45")))],
    );
    let res = h.post("tok-a", "/zen/x", "3").send().await.unwrap();
    assert_ne!(res.status(), 200, "the first key is out of rotation");
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-b", "up-b"]);
}

#[tokio::test]
async fn one_request_disables_at_most_one_key() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    h.mock
        .script("up-b", vec![Reply::Status(401, "invalid api key", None)]);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(
        res.status(),
        401,
        "the upstream's own answer is handed back"
    );
    // Only the first key was disabled, so the second still serves the next request.
    let res = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-b"]);
}

#[tokio::test]
async fn hold_unauthorized_keeps_a_key_in_rotation_when_every_key_refuses_the_request() {
    let h = harness_with_upstream(
        &one_stream(),
        "hold_unauthorized = true\n",
        Policy::default(),
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    h.mock
        .script("up-b", vec![Reply::Status(401, "invalid api key", None)]);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(
        res.status(),
        401,
        "the upstream's own answer is handed back"
    );
    let res = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 200);
    // With the second key cooling, the first still serves: it was never disabled.
    h.mock.script(
        "up-b",
        vec![Reply::Status(429, "slow down", Some(("retry-after", "45")))],
    );
    let res = h.post("tok-a", "/zen/x", "3").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        h.mock.keys_called(),
        ["up-a", "up-b", "up-b", "up-b", "up-a"]
    );
}

#[tokio::test]
async fn hold_unauthorized_disables_the_key_once_a_later_key_serves_the_request() {
    let h = harness_with_upstream(
        &one_stream(),
        "hold_unauthorized = true\n",
        Policy::default(),
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-b"]);
}

#[tokio::test]
async fn an_upstream_401_disables_that_key() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-b"]);
}

#[tokio::test]
async fn a_bad_request_is_passed_through_without_failover() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(400, "{\"error\":\"bad json\"}", None)],
    );
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(res.text().await.unwrap(), "{\"error\":\"bad json\"}");
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn a_transient_upstream_error_is_not_replayed_on_the_same_key() {
    let h = harness(&[("client-a", "tok-a", &["a"][..])], Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(502, "bad gateway", None)]);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 502);
    assert_eq!(res.text().await.unwrap(), "bad gateway");
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn every_key_in_the_chain_is_tried_once_before_the_last_refusal_is_returned() {
    let streams: Vec<(&str, &str, &[&str])> =
        vec![("client-a", "tok-a", &["a", "b", "c", "d"][..])];
    let h = harness(&streams, Policy::default()).await;
    for (key, text) in [
        ("up-a", "a broke"),
        ("up-b", "b broke"),
        ("up-c", "c broke"),
        ("up-d", "d broke"),
    ] {
        h.mock.script(
            key,
            (0..4).map(|_| Reply::Status(500, text, None)).collect(),
        );
    }
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(
        h.mock.keys_called(),
        ["up-a", "up-b", "up-c", "up-d"],
        "each key must get one attempt"
    );
    assert_eq!(res.status(), 500);
    assert_eq!(res.text().await.unwrap(), "d broke");
}

#[tokio::test]
async fn returns_503_with_retry_after_when_the_whole_chain_is_exhausted() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(429, "slow down", Some(("retry-after", "45")))],
    );
    h.mock.script(
        "up-b",
        vec![Reply::Status(429, "slow down", Some(("retry-after", "20")))],
    );
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 503);
    let retry: u64 = res.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((19..=20).contains(&retry), "retry-after was {retry}");
    let body = res.text().await.unwrap();
    assert!(body.contains("reiki_exhausted"));
    assert!(
        !body.contains("up-a") && !body.contains("up-b"),
        "error leaked a key"
    );
}

#[tokio::test]
async fn a_limited_stream_gets_429_while_an_unlimited_one_shares_the_key_unchanged() {
    let streams: Vec<(&str, &str, &[&str])> = vec![
        ("limited", "tok-lim", &["a", "b"]),
        ("free", "tok-free", &["a", "b"]),
    ];
    let h = harness_with_limits(&streams, &[("limited", 2)], Policy::default()).await;

    for body in ["1", "2"] {
        let res = h.post("tok-lim", "/zen/x", body).send().await.unwrap();
        assert_eq!(res.status(), 200);
    }
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a"]);

    let res = h.post("tok-lim", "/zen/x", "3").send().await.unwrap();
    assert_eq!(res.status(), 429);
    let retry: u64 = res.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry >= 1, "retry-after was {retry}");
    let body = res.text().await.unwrap();
    assert!(body.contains("rate_limited"), "body was {body}");
    // The rejected request never reached the upstream.
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a"]);

    // A stream with no limit still serves the same first key, so the 429s left
    // the shared key's health and the chain position untouched.
    let res = h.post("tok-free", "/zen/x", "4").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a", "up-a"]);
}

#[tokio::test]
async fn a_failover_cannot_spend_more_attempts_than_the_limit_allows() {
    let h = harness_with_limits(&one_stream(), &[("client-a", 1)], Policy::default()).await;
    // The first key fails, so the relay would normally try key b. The one allowed
    // attempt is already spent, and key b must not be called.
    h.mock
        .script("up-a", vec![Reply::Status(500, "upstream broke", None)]);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 429);
    let retry: u64 = res.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry >= 1, "retry-after was {retry}");
    let body = res.text().await.unwrap();
    assert!(body.contains("rate_limited"), "body was {body}");
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn a_shaped_rpm_budget_waits_for_the_next_slot() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let h = harness_with_shaper(
        &one_stream(),
        "[upstream.zen.limits]\nrpm = 2\nmax_wait_secs = 5\n",
        now,
    )
    .await;
    for _ in 0..2 {
        assert_eq!(
            h.post("tok-a", "/zen/x", "1")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }

    // One second short of the first slot's expiry: the request must sleep for it.
    h.clock.set(now + Duration::from_secs(59));
    let started = Instant::now();
    let res = h.post("tok-a", "/zen/x", "3").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the third request did not wait: {:?}",
        started.elapsed()
    );
    assert_eq!(h.mock.keys_called().len(), 3);
}

#[tokio::test]
async fn a_wait_beyond_max_wait_is_shed_without_touching_key_health() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\nrpm = 1\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let res = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 429);
    assert_eq!(res.headers()["x-reiki-origin"], "local");
    let retry: u64 = res.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry), "retry-after was {retry}");
    let body = res.text().await.unwrap();
    assert!(body.contains("reiki_shaped"), "body was {body}");
    // The shed never reached an upstream and neither key was cooled or disabled.
    assert_eq!(h.mock.keys_called(), ["up-a"]);
    assert_eq!(admin_key_state(&h, "a").await, "available");
    assert_eq!(admin_key_state(&h, "b").await, "available");

    let admin = h.admin.as_ref().unwrap();
    let metrics = h
        .client
        .get(format!("{admin}/v1/metrics"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("reiki_rate_limit_rejections_total{stream=\"client-a\"} 1"),
        "shed was not counted: {metrics}"
    );
}

#[tokio::test]
async fn a_closed_daily_window_moves_to_the_next_key() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let h = harness_with_shaper(
        &one_stream(),
        "[upstream.zen.limits]\ndaily_requests = 1\nmax_wait_secs = 10\n",
        now,
    )
    .await;
    for body in ["1", "2"] {
        assert_eq!(
            h.post("tok-a", "/zen/x", body)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
    assert_eq!(admin_key_state(&h, "a").await, "available");
    assert_eq!(admin_key_state(&h, "b").await, "available");
}

#[tokio::test]
async fn a_concurrent_slot_is_held_until_the_first_body_ends() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let h = harness_with_shaper(
        &one_stream(),
        "[upstream.zen.limits]\nconcurrent = 1\nmax_wait_secs = 10\n",
        now,
    )
    .await;
    h.mock.script(
        "up-a",
        vec![Reply::Slow(
            vec!["data: one\n\n", "data: two\n\n"],
            Duration::from_millis(500),
        )],
    );

    let mut first = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(&first.chunk().await.unwrap().unwrap()[..], b"data: one\n\n");

    let second = h.post("tok-a", "/zen/x", "2").send();
    tokio::pin!(second);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut second)
            .await
            .is_err(),
        "the second request ran while the first body held the only slot"
    );
    while first.chunk().await.unwrap().is_some() {}
    drop(first);

    let res = tokio::time::timeout(Duration::from_secs(5), &mut second)
        .await
        .expect("the slot was not released after the body ended")
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn json_usage_corrects_daily_tokens_down() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\ndaily_tokens = 100\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    let response = "{\"usage\":{\"prompt_tokens\":6,\"completion_tokens\":4}}";
    h.mock
        .script("up-a", vec![Reply::Headers(200, response, vec![])]);

    let first = h
        .post("tok-a", "/zen/x", &"x".repeat(400))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.text().await.unwrap(), response);

    // The estimate was 100; the response says 10, so another 50 still fits.
    let second = h
        .post("tok-a", "/zen/x", &"x".repeat(200))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        200,
        "usage did not correct the estimate down"
    );
}

#[tokio::test]
async fn json_usage_corrects_daily_tokens_up() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\ndaily_tokens = 100\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    let response = "{\"usage\":{\"input_tokens\":300,\"output_tokens\":200}}";
    h.mock
        .script("up-a", vec![Reply::Headers(200, response, vec![])]);

    let first = h
        .post("tok-a", "/zen/x", &"x".repeat(40))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.text().await.unwrap(), response);

    // The estimate was 10; usage says 500, so even one more token is over budget.
    let second = h
        .post("tok-a", "/zen/x", &"x".repeat(4))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        429,
        "usage did not correct the estimate up"
    );
    assert!(second.text().await.unwrap().contains("reiki_shaped"));
}

#[tokio::test]
async fn sse_usage_in_the_last_event_corrects_daily_tokens() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\ndaily_tokens = 100\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    let chunks = vec![
        "data: {\"choices\":[{\"delta\":{}}]}\n\n",
        "data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    ];
    h.mock.script(
        "up-a",
        vec![Reply::Slow(chunks.clone(), Duration::from_millis(5))],
    );

    let first = h
        .post("tok-a", "/zen/x", &"x".repeat(400))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.text().await.unwrap(), chunks.concat());

    // The estimate was 100; SSE usage says 7, so another 50 still fits.
    let second = h
        .post("tok-a", "/zen/x", &"x".repeat(200))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        200,
        "SSE usage did not correct the estimate"
    );
}

#[tokio::test]
async fn a_response_without_usage_keeps_the_estimate() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\ndaily_tokens = 100\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Headers(200, "{\"ok\":true}", vec![])]);

    let first = h
        .post("tok-a", "/zen/x", &"x".repeat(400))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.text().await.unwrap(), "{\"ok\":true}");

    // The estimate of 100 must remain; completing with zero would let this through.
    let second = h
        .post("tok-a", "/zen/x", &"x".repeat(4))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 429);
    assert!(second.text().await.unwrap().contains("reiki_shaped"));
}

#[tokio::test]
async fn a_client_disconnect_mid_sse_releases_the_slot() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\nconcurrent = 1\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    h.mock.script(
        "up-a",
        vec![Reply::Slow(
            vec!["data: one\n\n", "data: two\n\n"],
            Duration::from_millis(200),
        )],
    );

    let mut first = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(&first.chunk().await.unwrap().unwrap()[..], b"data: one\n\n");
    drop(first);

    let second = tokio::time::timeout(
        Duration::from_secs(3),
        h.post("tok-a", "/zen/x", "2").send(),
    )
    .await
    .expect("dropping the body did not release the concurrency slot")
    .unwrap();
    assert_eq!(second.status(), 200);
}

#[tokio::test]
async fn a_compressed_response_is_not_watched_for_usage() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\ndaily_tokens = 1000\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    let json = b"{\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1000}}";
    h.mock.script(
        "up-a",
        vec![Reply::Bytes(
            200,
            gzip(json),
            vec![("content-encoding", "gzip")],
        )],
    );

    let first = h
        .post("tok-a", "/zen/x", &"x".repeat(400))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.text().await.unwrap(), String::from_utf8_lossy(json));

    // The compressed body was decoded and buffered, so its usage must be ignored.
    let second = h
        .post("tok-a", "/zen/x", &"x".repeat(40))
        .send()
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        200,
        "a compressed body was watched for usage"
    );
}

#[tokio::test]
async fn a_provider_429_lowers_the_bucket_so_the_next_request_waits() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\nrpm = 10\nmax_wait_secs = 5\n",
        now,
    )
    .await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "1")),
        )],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        503
    );

    // Let the key's health cooldown pass, then jump to just before the learned
    // cap's next slot so the second request has to sleep for it.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    h.clock.set(now + Duration::from_secs(59));
    let started = Instant::now();
    let res = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the learned bucket did not delay the next request: {:?}",
        started.elapsed()
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a"]);
}

#[tokio::test]
async fn a_provider_429_is_saved_and_a_restart_waits_on_the_learned_bucket() {
    let state =
        std::env::temp_dir().join(format!("reiki-shaper-state-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let limits = "[upstream.zen.limits]\nrpm = 10\nmax_wait_secs = 5\n";

    let h = harness_with_shaper(&streams, limits, now).await;
    h.app.set_state_path(state.clone());
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "1")),
        )],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    let text = std::fs::read_to_string(&state).unwrap();
    assert!(text.contains("[shaper.a]"), "state was {text}");
    assert!(text.contains("pause_secs = 1"), "state was {text}");

    // Clear the key health so only the shaper's pause remains; enabling persists again.
    let admin = h.admin.as_ref().unwrap();
    assert_eq!(
        h.client
            .post(format!("{admin}/v1/keys/enable?key=a"))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let h2 = harness_with_shaper(&streams, limits, now).await;
    h2.app.set_state_path(state.clone());
    h2.app.load_persisted_state();
    let started = Instant::now();
    let res = h2.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "the restored pause did not delay the request: {:?}",
        started.elapsed()
    );
    assert_eq!(h2.mock.keys_called(), ["up-a"]);

    let _ = std::fs::remove_file(&state);
}

#[tokio::test]
async fn the_state_writer_saves_off_the_request_path_and_a_flush_waits_for_it() {
    let state =
        std::env::temp_dir().join(format!("reiki-state-writer-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let h = harness(&one_stream(), Policy::default()).await;
    h.app.set_state_path(state.clone());
    h.app.start_state_writer();
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 200, "the next key served the request");
    // The write happens on the writer thread, so give it a moment.
    let mut written = false;
    for _ in 0..60 {
        if std::fs::metadata(&state).is_ok_and(|m| m.len() > 0) {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(written, "the writer never saved the failure");

    // A flush does not return before the file is current.
    let _ = std::fs::remove_file(&state);
    assert!(h.app.enable_key("a"));
    h.app.flush_state();
    assert!(
        std::fs::metadata(&state).is_ok_and(|m| m.len() > 0),
        "flush_state returned before the state was on disk"
    );
    let _ = std::fs::remove_file(&state);
}

#[tokio::test]
async fn a_burst_of_failures_does_not_block_on_the_state_writer() {
    let state = std::env::temp_dir().join(format!("reiki-state-burst-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let h = harness(&one_stream(), Policy::default()).await;
    h.app.set_state_path(state.clone());
    h.app.start_state_writer();
    for _ in 0..20 {
        h.mock.script(
            "up-a",
            vec![Reply::Status(
                429,
                "Rate limit exceeded",
                Some(("retry-after", "1")),
            )],
        );
        let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
        assert!(res.status().is_success() || res.status() == 503);
    }
    h.app.flush_state();
    assert!(std::fs::metadata(&state).is_ok_and(|m| m.len() > 0));
    let _ = std::fs::remove_file(&state);
}

#[tokio::test]
async fn a_response_reporting_no_remaining_requests_closes_the_window() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\nrpm = 100\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    h.mock.script(
        "up-a",
        vec![Reply::Headers(
            200,
            "{\"ok\":true}",
            vec![
                ("x-ratelimit-remaining-requests", "0"),
                ("x-ratelimit-reset-requests", "60"),
            ],
        )],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let res = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.text().await.unwrap().contains("reiki_shaped"));
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn a_quota_402_closes_the_bucket_for_the_same_cooldown_as_the_key() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(
        &streams,
        "[upstream.zen.limits]\nrpm = 100\nmax_wait_secs = 1\n",
        now,
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Status(402, "quota", None)]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        503
    );

    // Clearing the health cooldown must not reopen the learned outbound window.
    let admin = h.admin.as_ref().unwrap();
    assert_eq!(
        h.client
            .post(format!("{admin}/v1/keys/enable?key=a"))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let res = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.text().await.unwrap().contains("reiki_shaped"));
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn reload_keeps_the_bucket_of_an_unchanged_key() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let limits = "[upstream.zen.limits]\nrpm = 1\nmax_wait_secs = 1\n";
    let streams: Vec<(&str, &str, &[&str])> = vec![("client-a", "tok-a", &["a"][..])];
    let h = harness_with_shaper(&streams, limits, now).await;
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    h.reload_shaped(&[("a", "up-a")], &streams, limits);
    let res = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(res.status(), 429, "the reload reset the bucket");
    assert!(res.text().await.unwrap().contains("reiki_shaped"));
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn an_upstream_without_limits_is_not_shaped() {
    let h = harness(&one_stream(), Policy::default()).await;
    for body in ["1", "2", "3"] {
        assert_eq!(
            h.post("tok-a", "/zen/x", body)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a", "up-a"]);
}

#[tokio::test]
async fn rejects_unknown_tokens_and_unknown_upstreams_without_calling_upstream() {
    let h = harness(&one_stream(), Policy::default()).await;
    assert_eq!(
        h.post("wrong", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let no_auth = h
        .client
        .post(format!("{}/zen/x", h.relay))
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), 401);
    assert_eq!(
        h.post("tok-a", "/nope/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert!(h.mock.calls().is_empty());
}

#[tokio::test]
async fn health_endpoint_needs_no_token() {
    let h = harness(&one_stream(), Policy::default()).await;
    let res = h
        .client
        .get(format!("{}/__reiki/healthz", h.relay))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn streams_the_response_instead_of_buffering_it() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Slow(
            vec!["data: one\n\n", "data: two\n\n", "data: three\n\n"],
            Duration::from_millis(300),
        )],
    );
    let started = Instant::now();
    let mut res = h
        .post("tok-a", "/zen/chat/completions", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let first = res.chunk().await.unwrap().unwrap();
    let first_at = started.elapsed();
    assert_eq!(&first[..], b"data: one\n\n");
    assert!(
        first_at < Duration::from_millis(250),
        "first chunk took {first_at:?}; the body was buffered"
    );
    let mut rest = Vec::new();
    while let Some(chunk) = res.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(
        String::from_utf8(rest).unwrap(),
        "data: two\n\ndata: three\n\n"
    );
    assert!(started.elapsed() >= Duration::from_millis(550));
}

#[tokio::test]
async fn streams_with_disjoint_keys_do_not_affect_each_other() {
    let streams: Vec<(&str, &str, &[&str])> =
        vec![("one", "tok-1", &["a", "b"]), ("two", "tok-2", &["c", "d"])];
    let h = harness(&streams, Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(429, "Free usage exceeded", None)],
    );
    assert_eq!(
        h.post("tok-1", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.post("tok-2", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-c"]);
}

#[tokio::test]
async fn streams_sharing_a_key_share_its_cooldown() {
    let streams: Vec<(&str, &str, &[&str])> =
        vec![("one", "tok-1", &["a", "b"]), ("two", "tok-2", &["a", "d"])];
    let h = harness(&streams, Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(429, "Free usage exceeded", None)],
    );
    assert_eq!(
        h.post("tok-1", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.post("tok-2", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-d"]);
}

const DISABLED: &str = "{\"error\":{\"type\":\"server_error\",\"message\":\"Upstream request failed: Model access is disabled\"}}";

#[tokio::test]
async fn an_endpoint_specific_refusal_does_not_cool_the_key_for_other_endpoints() {
    let h = harness(&[("client-a", "tok-a", &["a"][..])], Policy::default()).await;
    h.mock.script_path(
        "up-a",
        "/v1/responses",
        vec![
            Reply::Status(403, DISABLED, None),
            Reply::Status(403, DISABLED, None),
        ],
    );
    let res = h
        .post("tok-a", "/zen/responses", "probe")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(res.text().await.unwrap(), DISABLED);
    let res = h
        .post("tok-a", "/zen/chat/completions", "real")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let res = h
        .post("tok-a", "/zen/responses", "probe2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a", "up-a"]);
}

#[tokio::test]
async fn a_refusal_on_one_key_moves_to_the_next_key_for_that_request_only() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock.script_path(
        "up-a",
        "/v1/responses",
        vec![Reply::Status(403, DISABLED, None)],
    );
    let res = h.post("tok-a", "/zen/responses", "1").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
}

#[tokio::test]
async fn rejects_a_request_body_over_the_size_limit_without_calling_upstream() {
    let h = harness(&one_stream(), Policy::default()).await;
    let oversized = "x".repeat(8 * 1024 * 1024 + 1);
    let res = h.post("tok-a", "/zen/x", &oversized).send().await.unwrap();
    assert_eq!(res.status(), 413);
    assert!(h.mock.calls().is_empty());
}

#[tokio::test]
async fn an_idempotent_request_that_never_gets_headers_is_retried_as_a_transient_failure() {
    let h = harness_with_timeout(
        &[("client-a", "tok-a", &["a"][..])],
        Policy::default(),
        Duration::from_millis(200),
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Delay(Duration::from_secs(5))]);
    let res = h
        .client
        .get(format!("{}/zen/x", h.relay))
        .bearer_auth("tok-a")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a"]);
}

#[tokio::test]
async fn a_post_that_times_out_is_not_replayed_on_the_same_or_another_key() {
    let h = harness_with_timeout(
        &[("client-a", "tok-a", &["a", "b"][..])],
        Policy::default(),
        Duration::from_millis(200),
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Delay(Duration::from_secs(5))]);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    // The request may have reached the upstream, so a second key is not tried either.
    assert_eq!(res.status(), 504);
    assert_eq!(h.mock.keys_called(), ["up-a"]);
    let body = res.text().await.unwrap();
    assert!(body.contains("gateway_timeout"), "body was {body}");
    assert!(!body.contains("up-a"), "the timeout body leaked a key");
}

fn imf_date(epoch_secs: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    let weekday = WEEKDAYS[(((days % 7) + 7 + 4) % 7) as usize];
    format!(
        "{weekday}, {day:02} {} {year:04} {hour:02}:{minute:02}:{second:02} GMT",
        MONTHS[(month - 1) as usize]
    )
}

#[tokio::test]
async fn a_429_with_an_http_date_retry_after_cools_the_key_for_that_long() {
    let h = harness(&[("client-a", "tok-a", &["a"][..])], Policy::default()).await;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let when: &'static str = Box::leak(imf_date(now + 20).into_boxed_str());
    h.mock.script(
        "up-a",
        vec![Reply::Status(429, "slow down", Some(("retry-after", when)))],
    );
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 503);
    let retry: u64 = res.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((18..=20).contains(&retry), "retry-after was {retry}");
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn reload_adds_a_stream_and_a_key() {
    let h = harness(&[("client-a", "tok-a", &["a"][..])], Policy::default()).await;
    h.reload(
        &[("a", "up-a"), ("e", "up-e")],
        &[("client-a", "tok-a", &["a"]), ("new", "tok-new", &["e"])],
    );
    let res = h.post("tok-new", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-e"]);
}

#[tokio::test]
async fn reload_with_an_invalid_config_keeps_the_old_snapshot_serving() {
    let h = harness(&one_stream(), Policy::default()).await;
    let path = std::env::temp_dir().join(format!("reiki-bad-{}.toml", std::process::id()));
    std::fs::write(&path, "listen = 5\nthis is not toml").unwrap();
    assert!(h.app.reload_from_file(&path).is_err());
    let _ = std::fs::remove_file(&path);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn reload_keeps_health_for_an_unchanged_key() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);

    h.reload(
        &[("a", "up-a"), ("b", "up-b")],
        &[("client-a", "tok-a", &["a", "b"])],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-b"]);
}

#[tokio::test]
async fn reload_resets_health_when_a_key_secret_changes() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);

    h.reload(
        &[("a", "up-a-rotated"), ("b", "up-b")],
        &[("client-a", "tok-a", &["a", "b"])],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-a-rotated"]);
}

#[tokio::test]
async fn an_in_flight_request_finishes_against_the_snapshot_it_started_with() {
    let h = harness(&one_stream(), Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Slow(
            vec!["data: one\n\n", "data: two\n\n", "data: three\n\n"],
            Duration::from_millis(300),
        )],
    );
    let mut res = h
        .post("tok-a", "/zen/chat/completions", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let first = res.chunk().await.unwrap().unwrap();
    assert_eq!(&first[..], b"data: one\n\n");

    h.reload(&[("b", "up-b")], &[("client-a", "tok-a", &["b"])]);

    let mut rest = Vec::new();
    while let Some(chunk) = res.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(
        String::from_utf8(rest).unwrap(),
        "data: two\n\ndata: three\n\n"
    );

    let res = h
        .post("tok-a", "/zen/chat/completions", "2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
}

#[tokio::test]
async fn admin_endpoints_require_the_bearer_token() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    let admin = h.admin.as_ref().unwrap();
    for path in ["/v1/status", "/v1/metrics"] {
        let no_auth = h.client.get(format!("{admin}{path}")).send().await.unwrap();
        assert_eq!(no_auth.status(), 401, "{path} without a token");
        let wrong = h
            .client
            .get(format!("{admin}{path}"))
            .bearer_auth("not-the-token")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401, "{path} with a wrong token");
        let ok = h
            .client
            .get(format!("{admin}{path}"))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200, "{path} with the token");
    }
}

#[tokio::test]
async fn metrics_is_absent_unless_the_plugin_is_configured() {
    let h = harness_with_admin_opts(&one_stream(), "admin-secret", Policy::default(), false).await;
    let admin = h.admin.as_ref().unwrap();
    let get = |token: Option<&'static str>| {
        let mut req = h.client.get(format!("{admin}/v1/metrics"));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send()
    };
    assert_eq!(get(None).await.unwrap().status(), 401);
    assert_eq!(get(Some("admin-secret")).await.unwrap().status(), 404);
    let status = h
        .client
        .get(format!("{admin}/v1/status"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
}

#[tokio::test]
async fn the_metrics_plugin_leaves_content_length_on_responses() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        res.headers().contains_key("content-length"),
        "{:?}",
        res.headers()
    );
}

#[tokio::test]
async fn admin_status_reports_key_and_stream_state_without_secrets() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "30")),
        )],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let admin = h.admin.as_ref().unwrap();
    let body = h
        .client
        .get(format!("{admin}/v1/status"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("\"name\":\"client-a\""), "body was {body}");
    assert!(body.contains("\"state\":\"available\""), "body was {body}");
    assert!(body.contains("\"state\":\"cooling\""), "body was {body}");
    assert!(body.contains("\"retry_after_seconds\":"), "body was {body}");
    assert!(
        body.contains("\"cooldown_class\":\"rate_limited\""),
        "body was {body}"
    );
    assert!(body.contains("\"current_key\":\"b\""), "body was {body}");
    assert!(!body.contains("up-a"), "status leaked a key value");
    assert!(!body.contains("tok-a"), "status leaked a token");
    assert!(!body.contains(&sha("tok-a")), "status leaked a token hash");
    assert!(!body.contains("/unused/"), "status leaked a file path");
}

#[tokio::test]
async fn admin_metrics_increment_after_a_failover_without_leaking() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "30")),
        )],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let admin = h.admin.as_ref().unwrap();
    let text = h
        .client
        .get(format!("{admin}/v1/metrics"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        text.contains(
            "reiki_requests_total{stream=\"client-a\",upstream=\"zen\",status=\"success\"} 1"
        ),
        "text was {text}"
    );
    assert!(
        text.contains(
            "reiki_failovers_total{stream=\"client-a\",key=\"a\",class=\"rate_limited\"} 1"
        ),
        "text was {text}"
    );
    assert!(
        text.contains("reiki_cooldowns_total{key=\"a\",class=\"rate_limited\",scope=\"key\"} 1"),
        "text was {text}"
    );
    assert!(
        text.contains("reiki_keys{state=\"cooling\"} 1"),
        "text was {text}"
    );
    assert!(!text.contains("up-a"), "metrics leaked a key value");
    assert!(!text.contains(&sha("tok-a")), "metrics leaked a token hash");
    assert!(!text.contains("/unused/"), "metrics leaked a file path");
}

#[tokio::test]
async fn a_restart_keeps_a_cooling_key_cooling() {
    let state =
        std::env::temp_dir().join(format!("reiki-state-restart-{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&state);

    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    h.app.set_state_path(state.clone());
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "60")),
        )],
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);

    // A fresh process over the same state file must not reuse the cooled key.
    let h2 = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    h2.app.set_state_path(state.clone());
    h2.app.load_persisted_state();
    assert_eq!(
        h2.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h2.mock.keys_called(), ["up-b"]);

    let admin = h2.admin.as_ref().unwrap();
    let body = h2
        .client
        .get(format!("{admin}/v1/status"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("\"state\":\"cooling\""), "body was {body}");

    let _ = std::fs::remove_file(&state);
}

fn retry_after_seconds(body: &str, key: &str) -> u64 {
    let marker = format!("\"name\":\"{key}\"");
    let rest = &body[body.find(&marker).expect("key in status")..];
    let field = "\"retry_after_seconds\":";
    let value = &rest[rest.find(field).expect("retry_after_seconds in status") + field.len()..];
    value
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("a number")
}

#[tokio::test]
async fn a_quota_reset_schedule_cools_a_key_until_the_reset_not_the_default() {
    // 2026-01-01T23:59:57Z, so the next `daily 00:00 UTC` reset is 3 seconds out.
    let now = UNIX_EPOCH + Duration::from_secs(1_767_311_997);
    let h = harness_with_quota_resets(
        &[("client-a", "tok-a", &["a", "b"][..])],
        &[("a", "daily 00:00 UTC")],
        now,
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Status(402, "quota exhausted", None)]);
    h.mock
        .script("up-b", vec![Reply::Status(402, "quota exhausted", None)]);

    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);

    let admin = h.admin.as_ref().unwrap();
    let body = h
        .client
        .get(format!("{admin}/v1/status"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // `a` has a schedule, so it cools only until the reset; `b` has none and
    // keeps the fixed five-hour window.
    let a = retry_after_seconds(&body, "a");
    assert!(
        (1..=5).contains(&a),
        "key a cooled for {a}s; body was {body}"
    );
    let b = retry_after_seconds(&body, "b");
    assert!(
        (17_990..=18_000).contains(&b),
        "key b cooled for {b}s; body was {body}"
    );
    assert_eq!(
        body.matches("\"cooldown_class\":\"quota_exhausted\"")
            .count(),
        2,
        "body was {body}"
    );
}

#[tokio::test]
async fn a_sighup_reload_does_not_revive_the_old_state_of_a_rotated_key() {
    let dir = std::env::temp_dir().join(format!("reiki-hup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("reiki.toml.state");

    let h = harness(&one_stream(), Policy::default()).await;
    h.app.set_state_path(state.clone());

    // Key files exist before the failure is persisted, so the state file is newer
    // than they are and would normally be trusted on a replay.
    let write_config = |a_secret: &str| {
        write_key(dir.join("a"), a_secret);
        // At least 8 bytes, or the reload refuses the file at load.
        write_key(dir.join("b"), "upstream-b");
        let text = format!(
            "[upstream.zen]\nbase_url = \"{}/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.b]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [streams.client-a]\ntoken_sha256 = \"{}\"\nchain = [\"a\", \"b\"]\n",
            h.mock_url,
            dir.join("a").display(),
            dir.join("b").display(),
            sha("tok-a")
        );
        let path = dir.join("reiki.toml");
        std::fs::write(&path, text).unwrap();
        path
    };
    write_config("up-a");
    std::thread::sleep(Duration::from_millis(30));

    h.mock
        .script("up-a", vec![Reply::Status(401, "invalid api key", None)]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
    assert!(state.exists(), "the 401 should have been persisted");

    // Rotate the secret on disk, then reload as SIGHUP does.
    let path = write_config("up-a-rotated");
    h.app.reload_on_hangup(&path);

    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b", "up-a-rotated"]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_reload_naming_an_unknown_plugin_is_refused_and_the_old_config_keeps_serving() {
    let dir = std::env::temp_dir().join(format!("reiki-badplugin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = harness(&one_stream(), Policy::default()).await;

    let write_config = |a_secret: &str, extra: &str| {
        write_key(dir.join("a"), a_secret);
        // At least 8 bytes, or the reload refuses the file at load.
        write_key(dir.join("b"), "upstream-b");
        let text = format!(
            "[upstream.zen]\nbase_url = \"{}/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.b]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [streams.client-a]\ntoken_sha256 = \"{}\"\nchain = [\"a\", \"b\"]\n{extra}",
            h.mock_url,
            dir.join("a").display(),
            dir.join("b").display(),
            sha("tok-a")
        );
        let path = dir.join("reiki.toml");
        std::fs::write(&path, text).unwrap();
        path
    };

    let path = write_config("up-a-rotated", "[plugins.not-a-plugin]\n");
    h.app.reload_on_hangup(&path);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.mock.keys_called(),
        ["up-a"],
        "the refused reload must not have applied the rotated key"
    );

    let path = write_config("up-a-rotated", "");
    h.app.reload_on_hangup(&path);
    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a-rotated"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_402_with_no_model_cools_the_key() {
    let h =
        harness_with_upstream(&[("client-a", "tok-a", &["a"][..])], "", Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(402, "no payment method", None)]);
    let first = h
        .post("tok-a", "/zen/x", "1")
        .send()
        .await
        .unwrap()
        .status();
    assert_ne!(first, 200);
    let second = h
        .post("tok-a", "/zen/x", "2")
        .send()
        .await
        .unwrap()
        .status();
    assert_ne!(
        second, 200,
        "the key is cooling, so nothing reaches the upstream"
    );
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn confirm_quota_sends_a_quota_429_again_on_the_same_key_before_cooling_it() {
    let quota = || Reply::Status(429, "Free usage exceeded, subscribe to Go", None);
    let h = harness_with_upstream(&one_stream(), "confirm_quota = true\n", Policy::default()).await;
    h.mock.script("up-a", vec![quota()]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.mock.keys_called(),
        ["up-a", "up-a"],
        "one more try on the same key"
    );
    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called()[2], "up-a", "the key was never cooled");

    let h = harness_with_upstream(&one_stream(), "confirm_quota = true\n", Policy::default()).await;
    h.mock.script("up-a", vec![quota(), quota()]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.mock.keys_called(),
        ["up-a", "up-a", "up-b"],
        "a repeated quota 429 cools the key and moves on"
    );
}

#[tokio::test]
async fn pass_errors_relays_a_403_without_taking_the_key_out_of_rotation() {
    let h = harness_with_upstream(&one_stream(), "pass_errors = true\n", Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(403, "forbidden", None)]);
    let denied = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(
        denied.status(),
        403,
        "the upstream's own answer reaches the client"
    );
    assert_eq!(h.mock.keys_called(), ["up-a"], "no replay on another key");
    let again = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(again.status(), 200);
    assert_eq!(
        h.mock.keys_called(),
        ["up-a", "up-a"],
        "the key stayed in rotation"
    );

    let h = harness(&one_stream(), Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(403, "forbidden", None)]);
    h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(
        h.mock.keys_called()[..2],
        ["up-a", "up-b"],
        "without the option a 403 still fails over to the next key"
    );
}

#[tokio::test]
async fn a_relayed_error_body_over_the_cap_is_cut_and_marked() {
    let h = harness_with_upstream(&one_stream(), "pass_errors = true\n", Policy::default()).await;
    let big = vec![b'e'; 70 * 1024];
    h.mock.script("up-a", vec![Reply::Bytes(400, big, vec![])]);
    let cut = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(cut.status(), 400);
    assert_eq!(
        cut.headers()
            .get("x-reiki-truncated")
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "a cut body says so"
    );
    let body = cut.bytes().await.unwrap();
    assert_eq!(body.len(), 64 * 1024);

    h.mock
        .script("up-a", vec![Reply::Status(400, "small error", None)]);
    let whole = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert!(whole.headers().get("x-reiki-truncated").is_none());
    assert_eq!(whole.text().await.unwrap(), "small error");
}

#[tokio::test]
async fn a_generic_403_does_not_permanently_disable_a_key() {
    let h = harness(&[("client-a", "tok-a", &["a"][..])], Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            403,
            r#"{"error":{"message":"request blocked by security policy"}}"#,
            None,
        )],
    );
    let denied = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(
        denied.status(),
        403,
        "the upstream's own answer still reaches the client"
    );
    let again = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(again.status(), 200, "the key was not permanently disabled");
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a"]);
}

#[tokio::test]
async fn a_403_naming_an_invalid_key_still_disables_it() {
    let h = harness(&[("client-a", "tok-a", &["a"][..])], Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            403,
            r#"{"error":{"message":"API key is invalid"}}"#,
            None,
        )],
    );
    let denied = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(denied.status(), 503, "no key can serve the request");
    let exhausted = h.post("tok-a", "/zen/x", "2").send().await.unwrap();
    assert_eq!(exhausted.status(), 503, "the key stays disabled");
    assert_eq!(h.mock.keys_called(), ["up-a"]);
}

#[tokio::test]
async fn admin_routes_answer_under_v1_and_carry_the_version_header() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    let admin = h.admin.as_ref().unwrap();
    for path in ["/v1/status", "/v1/metrics"] {
        let res = h
            .client
            .get(format!("{admin}{path}"))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{path}");
        assert_eq!(
            res.headers()["x-reiki-version"],
            env!("CARGO_PKG_VERSION"),
            "{path}"
        );
    }
    for path in ["/status", "/metrics"] {
        let bare = h
            .client
            .get(format!("{admin}{path}"))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(bare.status(), 404, "{path}");
    }
    let refused = h
        .client
        .get(format!("{admin}/v1/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    assert!(refused.headers().contains_key("x-reiki-version"));
}

#[tokio::test]
async fn the_openapi_document_lists_exactly_the_v1_routes_that_answer() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    let admin = h.admin.as_ref().unwrap();
    let url = format!("{admin}/v1/openapi.json");
    assert_eq!(h.client.get(&url).send().await.unwrap().status(), 401);
    let text = h
        .client
        .get(&url)
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(doc["openapi"].as_str().unwrap().starts_with("3.1"));
    assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object());
    let paths = doc["paths"].as_object().unwrap();
    assert!(paths["/v1/reload"]["post"]["parameters"][0]["name"] == "dry_run");
    for (path, item) in paths {
        let method = item.as_object().unwrap().keys().next().unwrap().clone();
        let request = if method == "post" {
            // A dry run changes nothing.
            h.client.post(format!("{admin}{path}?dry_run=true"))
        } else {
            h.client.get(format!("{admin}{path}"))
        };
        let status = request
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status();
        assert_ne!(status, 404, "documented but not served: {path}");
    }
    for served in [
        "/v1/status",
        "/v1/reload",
        "/v1/metrics",
        "/v1/openapi.json",
    ] {
        assert!(
            paths.contains_key(served),
            "served but not documented: {served}"
        );
    }
}

/// The `state` one key reports in `/status`: available, cooling or out_of_rotation.
async fn admin_key_state(h: &Harness, name: &str) -> String {
    let admin = h.admin.as_ref().unwrap();
    let body = h
        .client
        .get(format!("{admin}/v1/status"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    doc["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["name"] == name)
        .unwrap()["state"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn admin_keys_enable_re_enables_a_disabled_key() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(
            429,
            "Rate limit exceeded",
            Some(("retry-after", "30")),
        )],
    );
    // 429 cools the key and fails over to the next one.
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let admin = h.admin.as_ref().unwrap();
    assert_eq!(admin_key_state(&h, "a").await, "cooling");

    let post = |path: &'static str, token: Option<&'static str>| {
        let mut req = h.client.post(format!("{admin}{path}"));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send()
    };

    assert_eq!(
        post("/v1/keys/enable?key=a", None).await.unwrap().status(),
        401
    );
    assert_eq!(
        post("/v1/keys/enable?key=a", Some("not-the-token"))
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        post("/v1/keys/enable", Some("admin-secret"))
            .await
            .unwrap()
            .status(),
        400,
        "a missing key name answers 400"
    );
    assert_eq!(
        post("/v1/keys/enable?key=%zz", Some("admin-secret"))
            .await
            .unwrap()
            .status(),
        400,
        "an undecodable name answers 400"
    );
    assert_eq!(
        post("/v1/keys/enable?key=zzz", Some("admin-secret"))
            .await
            .unwrap()
            .status(),
        404,
        "an unknown key answers 404"
    );

    let ok = post("/v1/keys/enable?key=a", Some("admin-secret"))
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.text().await.unwrap(), "{\"ok\":true,\"key\":\"a\"}");
    assert_eq!(admin_key_state(&h, "a").await, "available");

    // The key serves traffic again.
    assert_eq!(
        h.post("tok-a", "/zen/x", "2")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn usage_needs_the_admin_token_and_answers_under_v1() {
    let h = harness_with_admin(&one_stream(), "admin-secret", Policy::default()).await;
    let admin = h.admin.as_ref().unwrap();
    let refused = h
        .client
        .get(format!("{admin}/v1/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    let ok = h
        .client
        .get(format!("{admin}/v1/usage"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.headers()["content-type"], "application/json");
    let bare = h
        .client
        .get(format!("{admin}/usage"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), 404);
}
