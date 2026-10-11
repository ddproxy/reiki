use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    relay: String,
    client: reqwest::Client,
}

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
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

    let mut text = format!("[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n");
    let mut keys = HashMap::new();
    for k in ["a", "b", "c", "d"] {
        text.push_str(&format!(
            "[keys.{k}]\nupstream = \"zen\"\nfile = \"/unused/{k}\"\n"
        ));
        keys.insert(k.to_owned(), Secret::new(format!("up-{k}")));
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
    let relay = serve(router(App::with_header_timeout(
        config,
        keys,
        policy,
        header_timeout,
    )))
    .await;
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
            .header("user-agent", "opencode/1.18.35")
            .header("x-opencode-session", "ses_123")
            .body(body.to_owned())
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
