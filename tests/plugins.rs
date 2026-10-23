use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{PluginChain, Registration};
use reiki::proxy::{router, App, Secret};
use reiki::reiki_plugin::{ChunkCtx, Plugin, PluginError, RequestCtx, ResponseHeadCtx};

#[derive(Clone, Debug)]
struct Call {
    authorization: String,
    body: Vec<u8>,
}

enum Reply {
    Status(u16, &'static str, Option<(&'static str, &'static str)>),
    Slow(Vec<&'static str>, Duration),
}

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<Call>>,
    script: Mutex<HashMap<String, VecDeque<Reply>>>,
}

impl Mock {
    fn script(&self, key: &str, replies: Vec<Reply>) {
        self.script
            .lock()
            .unwrap()
            .insert(key.to_owned(), replies.into());
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
    let authorization = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    mock.calls.lock().unwrap().push(Call {
        authorization: authorization.clone(),
        body,
    });
    let key = authorization.trim_start_matches("Bearer ").to_owned();
    let reply = mock
        .script
        .lock()
        .unwrap()
        .get_mut(&key)
        .and_then(VecDeque::pop_front);
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
    relay: String,
    client: reqwest::Client,
}

fn config_text(
    mock_url: &str,
    keys: &[(&str, &str)],
    streams: &[(&str, &str, &[&str])],
    extra: &str,
) -> String {
    let mut text = format!("[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n");
    for (name, _) in keys {
        text.push_str(&format!(
            "[keys.{name}]\nupstream = \"zen\"\nfile = \"/unused/{name}\"\n"
        ));
    }
    for (name, token, chain) in streams {
        let chain = chain
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "[streams.{name}]\ntoken_sha256 = \"{}\"\nchain = [{chain}]\n",
            reiki::auth::sha256_hex(token)
        ));
    }
    text.push_str(extra);
    text
}

const TEST_KEYS: [(&str, &str); 4] = [("a", "up-a"), ("b", "up-b"), ("c", "up-c"), ("d", "up-d")];

async fn harness_with_plugins(
    streams: &[(&str, &str, &[&str])],
    policy: Policy,
    plugin_tables: &str,
    registry: Vec<Registration>,
) -> Harness {
    let mock = Arc::new(Mock::default());
    let mock_url = serve(
        Router::new()
            .fallback(mock_handler)
            .with_state(mock.clone()),
    )
    .await;

    let config =
        Config::parse(&config_text(&mock_url, &TEST_KEYS, streams, plugin_tables)).unwrap();
    let chain = PluginChain::build(&config.plugins, &registry).unwrap();
    let keys = TEST_KEYS
        .iter()
        .map(|(name, secret)| ((*name).to_owned(), Secret::new(*secret)))
        .collect();
    let app = App::with_plugins(config, keys, policy, Duration::from_secs(120), chain);
    let relay = serve(router(app)).await;
    Harness {
        mock,
        relay,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    fn post(&self, token: &str, path: &str, body: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.relay))
            .bearer_auth(token)
            .body(body.to_owned())
    }
}

fn one_stream() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    vec![("ct", "tok-a", &["a", "b"][..])]
}
const PLACEHOLDER: &str = "<SECRET>";
const SECRET: &str = "sk-real";

/// Replaces the secret with a placeholder on the way out and back in, holding
/// response chunks until the last one so a split placeholder is reassembled.
struct SwapSecrets;
impl Plugin for SwapSecrets {
    fn name(&self) -> &str {
        "swap"
    }
    fn new_state(&self) -> Box<dyn Any + Send> {
        Box::new(Vec::<u8>::new())
    }
    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let text = String::from_utf8_lossy(ctx.body).replace(SECRET, PLACEHOLDER);
        *ctx.body = Bytes::from(text);
        Ok(())
    }
    fn on_response_head(&self, ctx: &mut ResponseHeadCtx<'_>) {
        ctx.headers.remove(axum::http::header::CONTENT_LENGTH);
    }
    fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
        let buffer = ctx.state.downcast_mut::<Vec<u8>>().unwrap();
        buffer.extend_from_slice(&ctx.chunk[..]);
        if ctx.is_last {
            let text = String::from_utf8_lossy(buffer).replace(PLACEHOLDER, SECRET);
            *ctx.chunk = Bytes::from(text);
            buffer.clear();
        } else {
            *ctx.chunk = Bytes::new();
        }
    }
}

struct Marker;
impl Plugin for Marker {
    fn name(&self) -> &str {
        "marker"
    }
    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        let mut body = ctx.body.to_vec();
        body.extend_from_slice(b"-x");
        *ctx.body = Bytes::from(body);
        Ok(())
    }
}

struct Rejecter;
impl Plugin for Rejecter {
    fn name(&self) -> &str {
        "rejecter"
    }
    fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        Err(PluginError::Reject(
            StatusCode::FORBIDDEN,
            "blocked by plugin".into(),
        ))
    }
}

struct Skipper;
impl Plugin for Skipper {
    fn name(&self) -> &str {
        "skipper"
    }
    fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        Err(PluginError::Skip("not applicable".into()))
    }
}

fn swap_registry() -> Vec<Registration> {
    vec![Registration {
        name: "swap",
        build: |_| Ok(Box::new(SwapSecrets) as Box<dyn Plugin>),
    }]
}

fn marker_registry() -> Vec<Registration> {
    vec![Registration {
        name: "marker",
        build: |_| Ok(Box::new(Marker) as Box<dyn Plugin>),
    }]
}

fn reject_registry() -> Vec<Registration> {
    vec![Registration {
        name: "rejecter",
        build: |_| Ok(Box::new(Rejecter) as Box<dyn Plugin>),
    }]
}

fn skip_registry() -> Vec<Registration> {
    vec![Registration {
        name: "skipper",
        build: |_| Ok(Box::new(Skipper) as Box<dyn Plugin>),
    }]
}

#[tokio::test]
async fn a_plugin_swaps_a_secret_out_of_the_request_and_back_into_the_response() {
    let h = harness_with_plugins(
        &one_stream(),
        Policy::default(),
        "[plugins.swap]\n",
        swap_registry(),
    )
    .await;
    h.mock
        .script("up-a", vec![Reply::Status(200, "echo <SECRET> done", None)]);
    let res = h
        .post("tok-a", "/zen/chat/completions", "ask sk-real now")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "echo sk-real done");
    let calls = h.mock.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body, b"ask <SECRET> now");
}

#[tokio::test]
async fn a_placeholder_split_across_response_chunks_is_reassembled() {
    let h = harness_with_plugins(
        &one_stream(),
        Policy::default(),
        "[plugins.swap]\n",
        swap_registry(),
    )
    .await;
    h.mock.script(
        "up-a",
        vec![Reply::Slow(
            vec!["echo <SEC", "RET> done"],
            Duration::from_millis(10),
        )],
    );
    let res = h
        .post("tok-a", "/zen/chat/completions", "ask sk-real now")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "echo sk-real done");
}

#[tokio::test]
async fn a_plugin_rejection_returns_its_status_without_calling_upstream() {
    let h = harness_with_plugins(
        &one_stream(),
        Policy::default(),
        "[plugins.rejecter]\n",
        reject_registry(),
    )
    .await;
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 403);
    assert!(h.mock.calls().is_empty());
}

#[tokio::test]
async fn a_plugin_skip_passes_the_request_through_by_default() {
    let h = harness_with_plugins(
        &one_stream(),
        Policy::default(),
        "[plugins.skipper]\n",
        skip_registry(),
    )
    .await;
    let res = h.post("tok-a", "/zen/x", "hi").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.calls()[0].body, b"hi");
}

#[tokio::test]
async fn a_plugin_skip_rejects_when_on_error_is_reject() {
    let h = harness_with_plugins(
        &one_stream(),
        Policy::default(),
        "[plugins.skipper]\non_error = \"reject\"\n",
        skip_registry(),
    )
    .await;
    let res = h.post("tok-a", "/zen/x", "1").send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert!(h.mock.calls().is_empty());
}

#[tokio::test]
async fn the_request_hook_runs_once_and_failover_replays_the_transformed_body() {
    let h = harness_with_plugins(
        &one_stream(),
        Policy::default(),
        "[plugins.marker]\n",
        marker_registry(),
    )
    .await;
    h.mock.script(
        "up-a",
        vec![Reply::Status(429, "slow down", Some(("retry-after", "30")))],
    );
    let res = h.post("tok-a", "/zen/x", "hi").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(h.mock.keys_called(), ["up-a", "up-b"]);
    let bodies: Vec<Vec<u8>> = h.mock.calls().iter().map(|c| c.body.clone()).collect();
    assert_eq!(bodies, vec![b"hi-x".to_vec(), b"hi-x".to_vec()]);
}

struct HostileRejecter;
impl Plugin for HostileRejecter {
    fn name(&self) -> &str {
        "hostile"
    }
    fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        Err(PluginError::Reject(
            StatusCode::FORBIDDEN,
            "path C:\\temp\\x \"quoted\"\nsecond line\ttab \u{1}".into(),
        ))
    }
}

#[tokio::test]
async fn a_rejection_message_with_special_characters_is_valid_json() {
    let registry = vec![Registration {
        name: "hostile",
        build: |_| Ok(Box::new(HostileRejecter) as Box<dyn Plugin>),
    }];
    let h = harness_with_plugins(
        &one_stream(),
        Policy::default(),
        "[plugins.hostile]\n",
        registry,
    )
    .await;
    let res = h.post("tok-a", "/zen/x", "{}").send().await.unwrap();
    assert_eq!(res.status(), 403);
    let body: serde_json::Value = serde_json::from_str(&res.text().await.unwrap()).unwrap();
    assert_eq!(
        body["error"]["message"],
        "path C:\\temp\\x \"quoted\"\nsecond line\ttab \u{1}"
    );
    assert!(h.mock.calls().is_empty(), "the upstream must not be called");
}
