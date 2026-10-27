use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{router, App, Secret};

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
    Slow(Vec<&'static str>, Duration),
    Delay(Duration),
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

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

struct Harness {
    mock: Arc<Mock>,
    mock_url: String,
    relay: String,
    client: reqwest::Client,
    app: Arc<App>,
    admin: Option<String>,
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

async fn harness_with_admin(
    streams: &[(&str, &str, &[&str])],
    admin_token: &str,
    policy: Policy,
) -> Harness {
    harness_with_admin_opts(streams, admin_token, policy, true).await
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
    let app = App::with_plugins(config, key_map, policy, Duration::from_secs(120), plugins);
    let relay = serve(router(Arc::clone(&app))).await;
    let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
    Harness {
        mock,
        mock_url,
        relay,
        client: reqwest::Client::new(),
        app,
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
    let app = App::with_header_timeout(config, keys, Policy::default(), Duration::from_secs(120));
    app.set_wall_clock(now);
    let relay = serve(router(Arc::clone(&app))).await;
    let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
    Harness {
        mock,
        mock_url,
        relay,
        client: reqwest::Client::new(),
        app,
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
    let app = App::with_header_timeout(config, keys, policy, header_timeout);
    let relay = serve(router(Arc::clone(&app))).await;
    Harness {
        mock,
        mock_url,
        relay,
        client: reqwest::Client::new(),
        app,
        admin: None,
    }
}

impl Harness {
    fn post(&self, token: &str, path: &str, body: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.relay))
            .bearer_auth(token)
            .header("user-agent", "opencode/1.18.35")
            .header("x-opencode-session", "ses_123")
            .body(body.to_owned())
    }

    fn reload(&self, keys: &[(&str, &str)], streams: &[(&str, &str, &[&str])]) {
        let (config, keys) = config_for(&self.mock_url, keys, streams);
        self.app.reload(config, keys);
    }
}

fn one_stream() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    vec![("ct", "tok-a", &["a", "b"][..])]
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
    assert_eq!(calls[0].user_agent, "opencode/1.18.35");
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
async fn a_transient_error_is_retried_once_on_the_same_key() {
    let h = harness(&[("ct", "tok-a", &["a"][..])], Policy::default()).await;
    h.mock
        .script("up-a", vec![Reply::Status(502, "bad gateway", None)]);
    assert_eq!(
        h.post("tok-a", "/zen/x", "1")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a"]);
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
    let h = harness(&[("ct", "tok-a", &["a"][..])], Policy::default()).await;
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
async fn an_upstream_that_never_sends_headers_is_retried_as_a_transient_failure() {
    let h = harness_with_timeout(
        &[("ct", "tok-a", &["a"][..])],
        Policy::default(),
        Duration::from_millis(200),
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Delay(Duration::from_secs(5))]);
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-a"]);
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
    let h = harness(&[("ct", "tok-a", &["a"][..])], Policy::default()).await;
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
    let h = harness(&[("ct", "tok-a", &["a"][..])], Policy::default()).await;
    h.reload(
        &[("a", "up-a"), ("e", "up-e")],
        &[("ct", "tok-a", &["a"]), ("new", "tok-new", &["e"])],
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
    h.app.reload_from_file(&path);
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
        &[("ct", "tok-a", &["a", "b"])],
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
        &[("ct", "tok-a", &["a", "b"])],
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

    h.reload(&[("b", "up-b")], &[("ct", "tok-a", &["b"])]);

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
    for path in ["/status", "/metrics"] {
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
        let mut req = h.client.get(format!("{admin}/metrics"));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send()
    };
    assert_eq!(get(None).await.unwrap().status(), 401);
    assert_eq!(get(Some("admin-secret")).await.unwrap().status(), 404);
    let status = h
        .client
        .get(format!("{admin}/status"))
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
        .get(format!("{admin}/status"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("\"name\":\"ct\""), "body was {body}");
    assert!(body.contains("\"state\":\"available\""), "body was {body}");
    assert!(body.contains("\"state\":\"cooling\""), "body was {body}");
    assert!(body.contains("\"retry_after_seconds\":"), "body was {body}");
    assert!(body.contains("\"current_key\":\"b\""), "body was {body}");
    assert!(!body.contains("up-a"), "status leaked a key value: {body}");
    assert!(!body.contains("tok-a"), "status leaked a token: {body}");
    assert!(
        !body.contains(&sha("tok-a")),
        "status leaked a token hash: {body}"
    );
    assert!(
        !body.contains("/unused/"),
        "status leaked a file path: {body}"
    );
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
        .get(format!("{admin}/metrics"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        text.contains("reiki_requests_total{stream=\"ct\",upstream=\"zen\",status=\"success\"} 1"),
        "text was {text}"
    );
    assert!(
        text.contains("reiki_failovers_total{stream=\"ct\",key=\"a\",class=\"rate_limited\"} 1"),
        "text was {text}"
    );
    assert!(
        text.contains("reiki_cooldowns_total{key=\"a\",class=\"rate_limited\"} 1"),
        "text was {text}"
    );
    assert!(
        text.contains("reiki_keys{state=\"cooling\"} 1"),
        "text was {text}"
    );
    assert!(!text.contains("up-a"), "metrics leaked a key value: {text}");
    assert!(
        !text.contains(&sha("tok-a")),
        "metrics leaked a token hash: {text}"
    );
    assert!(
        !text.contains("/unused/"),
        "metrics leaked a file path: {text}"
    );
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
        .get(format!("{admin}/status"))
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
        &[("ct", "tok-a", &["a", "b"][..])],
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
        .get(format!("{admin}/status"))
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
        std::fs::write(dir.join("a"), a_secret).unwrap();
        std::fs::write(dir.join("b"), "up-b").unwrap();
        let text = format!(
            "[upstream.zen]\nbase_url = \"{}/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.b]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"a\", \"b\"]\n",
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
        std::fs::write(dir.join("a"), a_secret).unwrap();
        std::fs::write(dir.join("b"), "up-b").unwrap();
        let text = format!(
            "[upstream.zen]\nbase_url = \"{}/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.b]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"a\", \"b\"]\n{extra}",
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
