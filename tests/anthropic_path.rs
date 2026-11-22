//! Claude Code against a mock Anthropic Messages API: masking, thinking blocks, SSE restore,
//! deterministic history, cache markers and opaque binary blocks.
#![cfg(all(feature = "plugin-placeholders", unix))]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{builtin, PluginChain, RequestMeta};
use reiki::proxy::{router, App, Secret};
use serde_json::{json, Value};

mod common;
use common::serve;
use common::write_key;

const CLIENT_TOKEN: &str = "claude-code-client-token-0123456789";
const UPSTREAM_KEY: &str = "anthropic-upstream-key";

/// Assembled at runtime so no token-shaped literal sits in the source.
fn fake_secret() -> String {
    ["sk-ant-", "api03-", "Zx9Q2w8Er7Ty6Ui5Op4As3Df2Gh1Jk0L"].concat()
}

fn first_placeholder(text: &str) -> String {
    let start = text
        .find("[[REIKI_")
        .expect("a placeholder in the masked body");
    let end = text[start..].find("]]").unwrap() + start + 2;
    text[start..end].to_owned()
}

fn sse(name: &str, data: &str) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn sse_data(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str(payload).ok())
        .collect()
}

fn delta_texts(body: &str) -> Vec<String> {
    sse_data(body)
        .into_iter()
        .filter_map(|value| value["delta"]["text"].as_str().map(str::to_owned))
        .collect()
}

#[derive(Clone)]
struct Seen {
    headers: HeaderMap,
    body: String,
}

#[derive(Clone)]
enum Reply {
    Json(String),
    Sse(Vec<String>),
}

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<Seen>>,
    replies: Mutex<VecDeque<Reply>>,
}

/// Records the request, then answers from the queued replies.
async fn upstream(State(mock): State<Arc<Mock>>, req: Request) -> Response {
    let headers = req.headers().clone();
    let body = String::from_utf8(
        to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    mock.seen.lock().unwrap().push(Seen { headers, body });
    let reply = mock
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Reply::Json("{\"type\":\"message\",\"content\":[]}".to_owned()));
    match reply {
        Reply::Json(body) => Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
        Reply::Sse(events) => {
            let stream =
                futures_util::stream::unfold(events.into_iter(), |mut events| async move {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    events
                        .next()
                        .map(|event| (Ok::<_, std::io::Error>(Bytes::from(event)), events))
                });
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

struct Rig {
    relay: String,
    mock: Arc<Mock>,
    dir: PathBuf,
}

impl Rig {
    async fn start(tag: &str, plugins: &str) -> Self {
        Self::build(tag, plugins, false).await
    }

    async fn start_passthrough(tag: &str, plugins: &str) -> Self {
        Self::build(tag, plugins, true).await
    }

    async fn build(tag: &str, plugins: &str, passthrough: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("reiki-an-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("key-a");
        write_key(&key_file, format!("{UPSTREAM_KEY}\n"));

        let mock = Arc::new(Mock::default());
        let mock_url = serve(
            Router::new()
                .fallback(upstream)
                .with_state(Arc::clone(&mock)),
        )
        .await;

        let upstream_config = if passthrough {
            format!("[upstream.anthropic]\nbase_url = \"{mock_url}\"\npassthrough = true\n")
        } else {
            format!(
                "[upstream.anthropic]\nbase_url = \"{mock_url}\"\n\
                 auth_header = \"x-api-key\"\nauth_format = \"{{key}}\"\n\
                 [keys.a]\nupstream = \"anthropic\"\nfile = \"{}\"\n\
                 [streams.claude]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n",
                key_file.display(),
                reiki::auth::sha256_hex(CLIENT_TOKEN)
            )
        };
        let config = Config::parse(&format!("{upstream_config}{plugins}")).unwrap();
        let chain = PluginChain::build(&config.plugins, &builtin()).unwrap();
        let keys = if passthrough {
            HashMap::new()
        } else {
            [("a".to_owned(), Secret::new(UPSTREAM_KEY))]
                .into_iter()
                .collect()
        };
        let app = App::with_plugins(
            config,
            keys,
            Policy::default(),
            Duration::from_secs(30),
            chain,
        );
        Self {
            relay: serve(router(app)).await,
            mock,
            dir,
        }
    }

    async fn post(&self, body: &str) -> String {
        self.post_with(body, &[]).await
    }

    async fn post_with(&self, body: &str, extra: &[(&str, &str)]) -> String {
        let mut request = reqwest::Client::new()
            .post(format!("{}/anthropic/v1/messages", self.relay))
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");
        if !extra
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
        {
            request = request.header("x-api-key", CLIENT_TOKEN);
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        request
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    fn seen(&self, index: usize) -> Seen {
        self.mock.seen.lock().unwrap()[index].clone()
    }

    async fn enqueue_json(&self, body: String) {
        self.mock
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Json(body));
    }

    async fn enqueue_sse(&self, events: Vec<String>) {
        self.mock
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Sse(events));
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn request_masking_covers_string_blocks_and_tool_results() {
    let rig = Rig::start("mask", "[plugins.placeholders]\nterms = [\"Falcon\"]\n").await;
    let secret = fake_secret();
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": format!("my key is {secret}")},
            {"role": "user", "content": [
                {"type": "text", "text": format!("array form {secret} and Falcon")},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                    {"type": "text", "text": format!("tool output {secret}")}
                ]}
            ]}
        ]
    })
    .to_string();

    let _ = rig.post(&body).await;
    let sent = rig.seen(0);

    assert!(!sent.body.contains(&secret), "{}", sent.body);
    assert!(
        !sent.body.to_lowercase().contains("falcon"),
        "{}",
        sent.body
    );
    let parsed: Value = serde_json::from_str(&sent.body).unwrap();
    let placeholder = first_placeholder(&sent.body);
    assert_eq!(
        sent.body.matches(&placeholder).count(),
        3,
        "the same value masks to the same bytes: {}",
        sent.body
    );
    assert_eq!(
        parsed["messages"][0]["content"],
        format!("my key is {placeholder}")
    );
    assert!(parsed["messages"][1]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(&placeholder));
    assert_eq!(
        parsed["messages"][1]["content"][1]["content"][0]["text"],
        format!("tool output {placeholder}")
    );

    assert_eq!(sent.headers["x-api-key"], UPSTREAM_KEY);
    assert_eq!(sent.headers["anthropic-version"], "2023-06-01");
}

#[tokio::test]
async fn oauth_headers_pass_through_untouched() {
    let rig = Rig::start_passthrough(
        "headers",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\nbuiltin = false\n",
    )
    .await;

    let _ = rig
        .post_with(
            "{\"q\":\"Falcon\"}",
            &[
                ("x-api-key", "oauth-api-key"),
                ("authorization", "Bearer oauth-bearer"),
            ],
        )
        .await;
    let sent = rig.seen(0);

    assert_eq!(sent.headers["x-api-key"], "oauth-api-key");
    assert_eq!(sent.headers["authorization"], "Bearer oauth-bearer");
    assert!(!sent.body.contains("Falcon"), "{}", sent.body);
}

#[tokio::test]
async fn thinking_blocks_reach_the_upstream_byte_identical() {
    let rig = Rig::start(
        "thinking-request",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\n",
    )
    .await;
    let secret = fake_secret();
    let placeholder = "[[REIKI_SECRET_0123456789abcdef]]";
    let thinking = format!("weigh {secret} and {placeholder} carefully");
    let signature = format!("sig-{secret}-{placeholder}-==");
    let redacted = format!("opaque-{secret}-{placeholder}");
    let answer = format!("a normal answer with {secret}");
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "messages": [
            {"role": "user", "content": "start"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": thinking, "signature": signature},
                {"type": "redacted_thinking", "data": redacted},
                {"type": "text", "text": answer}
            ]},
            {"role": "user", "content": "continue"}
        ]
    })
    .to_string();

    let _ = rig.post(&body).await;
    let sent = rig.seen(0);
    let parsed: Value = serde_json::from_str(&sent.body).unwrap();
    let content = &parsed["messages"][1]["content"];

    assert_eq!(content[0]["thinking"], thinking);
    assert_eq!(content[0]["signature"], signature);
    assert_eq!(content[1]["data"], redacted);
    assert!(!content[2]["text"].as_str().unwrap().contains(&secret));
    assert!(content[2]["text"]
        .as_str()
        .unwrap()
        .contains("[[REIKI_SECRET_"));
}

#[tokio::test]
async fn thinking_blocks_are_not_restored_in_a_json_response() {
    let rig = Rig::start(
        "thinking-json",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\n",
    )
    .await;
    let secret = fake_secret();
    let request = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": format!("my key is {secret}")}]
    })
    .to_string();
    let _ = rig.post(&request).await;
    let placeholder = first_placeholder(&rig.seen(0).body);

    let thinking = format!("thought about {placeholder}");
    let signature = format!("signature {placeholder} ==");
    let data = format!("opaque {placeholder}");
    rig.enqueue_json(
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "thinking", "thinking": thinking, "signature": signature},
                {"type": "redacted_thinking", "data": data},
                {"type": "text", "text": format!("answer {placeholder}")}
            ],
            "stop_reason": "end_turn"
        })
        .to_string(),
    )
    .await;

    let reply = rig.post(&request).await;
    let parsed: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(parsed["content"][0]["thinking"], thinking);
    assert_eq!(parsed["content"][0]["signature"], signature);
    assert_eq!(parsed["content"][1]["data"], data);
    assert_eq!(parsed["content"][2]["text"], format!("answer {secret}"));
}

#[tokio::test]
async fn sse_thinking_and_signature_deltas_are_relayed_byte_identically() {
    let rig = Rig::start(
        "thinking-sse",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\n",
    )
    .await;
    let secret = fake_secret();
    let request = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": format!("my key is {secret}")}]
    })
    .to_string();
    let _ = rig.post(&request).await;
    let placeholder = first_placeholder(&rig.seen(0).body);

    let thinking = format!("thinking about {placeholder}");
    let signature = format!("signature {placeholder} ==");
    let redacted = format!("opaque {placeholder}");
    let events = vec![
        sse(
            "message_start",
            &json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant",
                "content": [], "model": "claude-sonnet-4-5"
            }})
            .to_string(),
        ),
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0, "content_block":
                {"type": "thinking", "thinking": "", "signature": ""}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "thinking_delta", "thinking": thinking}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "signature_delta", "signature": signature}})
            .to_string(),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}).to_string(),
        ),
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 1, "content_block":
                {"type": "redacted_thinking", "data": redacted}})
            .to_string(),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 1}).to_string(),
        ),
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 2, "content_block":
                {"type": "text", "text": ""}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &format!(
                "event: content_block_delta\nid: 7\ndata: {}\n\n",
                json!({"type": "content_block_delta", "index": 2, "delta":
                    {"type": "text_delta", "text": format!("visible {placeholder}")}})
            ),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 2}).to_string(),
        ),
        sse("message_stop", &json!({"type": "message_stop"}).to_string()),
    ];
    let names: Vec<String> = events
        .iter()
        .filter_map(|event| event.lines().next().map(str::to_owned))
        .collect();
    rig.enqueue_sse(events).await;

    let reply = rig.post(&request).await;
    let payloads = sse_data(&reply);

    let thinking_delta = payloads
        .iter()
        .find(|value| value["delta"]["type"] == "thinking_delta")
        .expect("a thinking delta");
    assert_eq!(thinking_delta["delta"]["thinking"], thinking);

    let signature_delta = payloads
        .iter()
        .find(|value| value["delta"]["type"] == "signature_delta")
        .expect("a signature delta");
    assert_eq!(signature_delta["delta"]["signature"], signature);

    let redacted_block = payloads
        .iter()
        .find(|value| value["content_block"]["type"] == "redacted_thinking")
        .expect("a redacted thinking block");
    assert_eq!(redacted_block["content_block"]["data"], redacted);

    let text_delta = payloads
        .iter()
        .find(|value| value["delta"]["type"] == "text_delta")
        .expect("a text delta");
    assert_eq!(text_delta["delta"]["text"], format!("visible {secret}"));

    let got_names: Vec<String> = reply
        .lines()
        .filter(|line| line.starts_with("event: "))
        .map(str::to_owned)
        .collect();
    assert_eq!(got_names, names, "SSE framing changed");
    assert!(reply.contains("id: 7\n"), "the SSE id was dropped");
    assert!(reply.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
}

#[tokio::test]
async fn a_second_turn_masks_history_to_the_same_placeholder() {
    let rig = Rig::start(
        "determinism",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\n",
    )
    .await;
    let secret = fake_secret();
    let turn1 = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": format!("remember {secret}")}]
    })
    .to_string();
    let _ = rig.post(&turn1).await;
    let placeholder = first_placeholder(&rig.seen(0).body);

    rig.enqueue_json(
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": format!("I will remember {placeholder}")}],
            "stop_reason": "end_turn"
        })
        .to_string(),
    )
    .await;
    let reply = rig.post(&turn1).await;
    assert!(
        reply.contains(&secret),
        "the client must receive the restored secret"
    );
    assert!(
        !reply.contains("REIKI"),
        "masked placeholders must not reach the client"
    );

    let turn2 = json!({
        "model": "claude-sonnet-4-5",
        "messages": [
            {"role": "user", "content": format!("remember {secret}")},
            {"role": "assistant", "content": [
                {"type": "text", "text": format!("I will remember {secret}")}
            ]},
            {"role": "user", "content": "what was it?"}
        ]
    })
    .to_string();
    let _ = rig.post(&turn2).await;
    let sent = rig.seen(2);
    let parsed: Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!(
        parsed["messages"][0]["content"],
        format!("remember {placeholder}")
    );
    assert_eq!(
        parsed["messages"][1]["content"][0]["text"],
        format!("I will remember {placeholder}"),
        "the resent assistant text must mask to the cached bytes"
    );
    assert!(!sent.body.contains(&secret), "{}", sent.body);
}

#[test]
fn a_reload_keeps_placeholder_ids() {
    let mut plugins = BTreeMap::new();
    let table: toml::Table = "terms = [\"Falcon\"]\n".parse().unwrap();
    plugins.insert("placeholders".to_owned(), table);
    let first = Arc::new(PluginChain::build(&plugins, &builtin()).unwrap());
    let second = Arc::new(PluginChain::build(&plugins, &builtin()).unwrap());
    let secret = fake_secret();
    let body = json!({
        "messages": [{"role": "user", "content": format!("key {secret}")}]
    })
    .to_string();

    let mask = |chain: &Arc<PluginChain>| {
        let mut states = chain.states();
        let mut headers = HeaderMap::new();
        let mut bytes = Bytes::from(body.clone());
        let method = Method::POST;
        let meta = RequestMeta {
            id: "",
            stream: "claude",
            upstream: "anthropic",
            method: &method,
            path: "/v1/messages",
        };
        chain
            .on_request(&mut states, &meta, &mut headers, &mut bytes)
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    };

    let one = mask(&first);
    let again = mask(&first);
    let reloaded = mask(&second);
    assert_eq!(one, again, "one plugin instance keeps its placeholder ids");
    assert_eq!(
        one, reloaded,
        "a reload in the same process must keep the placeholder ids for the prompt cache"
    );
    assert!(!one.contains(&secret));
    assert!(!reloaded.contains(&secret));
}

#[tokio::test]
async fn a_placeholder_split_across_sse_deltas_is_restored() {
    let rig = Rig::start(
        "split",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\nrestore_tool_args = true\n",
    )
    .await;
    let secret = fake_secret();
    let request = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": format!("my key is {secret}")}]
    })
    .to_string();
    let _ = rig.post(&request).await;
    let placeholder = first_placeholder(&rig.seen(0).body);
    let (head, tail) = placeholder.split_at(placeholder.len() / 2);

    let events = vec![
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0, "content_block":
                {"type": "text", "text": ""}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "text_delta", "text": format!("The token is {head}")}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "text_delta", "text": format!("{tail} and more")}})
            .to_string(),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}).to_string(),
        ),
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 1, "content_block":
                {"type": "tool_use", "id": "toolu_1", "name": "run", "input": {}}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 1, "delta":
                {"type": "input_json_delta", "partial_json": format!("{{\"command\":\"curl {head}")}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 1, "delta":
                {"type": "input_json_delta", "partial_json": format!("{tail}\"}}")}})
            .to_string(),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 1}).to_string(),
        ),
        sse(
            "message_stop",
            &json!({"type": "message_stop"}).to_string(),
        ),
    ];
    rig.enqueue_sse(events).await;

    let reply = rig.post(&request).await;
    let payloads = sse_data(&reply);
    let text: String = payloads
        .iter()
        .filter_map(|value| value["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, format!("The token is {secret} and more"));

    let json: String = payloads
        .iter()
        .filter_map(|value| value["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(json, format!("{{\"command\":\"curl {secret}\"}}"));
}

#[tokio::test]
async fn tool_argument_credentials_stay_masked_without_restore_tool_args() {
    let rig = Rig::start(
        "tool-masked",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\nrestore_tool_args = false\n",
    )
    .await;
    let secret = fake_secret();
    let request = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": format!("my key is {secret}")}]
    })
    .to_string();
    let _ = rig.post(&request).await;
    let placeholder = first_placeholder(&rig.seen(0).body);

    rig.enqueue_sse(vec![
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0, "content_block":
                {"type": "tool_use", "id": "toolu_1", "name": "run", "input": {}}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "input_json_delta", "partial_json": format!("{{\"command\":\"curl {placeholder}\"}}")}})
            .to_string(),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}).to_string(),
        ),
        sse(
            "message_stop",
            &json!({"type": "message_stop"}).to_string(),
        ),
    ])
    .await;

    let reply = rig.post(&request).await;
    let json: String = sse_data(&reply)
        .iter()
        .filter_map(|value| value["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(json, format!("{{\"command\":\"curl {placeholder}\"}}"));
    assert!(!json.contains(&secret));
}

#[tokio::test]
async fn a_held_partial_at_message_stop_is_flushed_before_it() {
    let rig = Rig::start(
        "held-stop",
        "[plugins.placeholders]\nterms = [\"Falcon\"]\n",
    )
    .await;
    let secret = fake_secret();
    let request = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": format!("my key is {secret}")}]
    })
    .to_string();
    let _ = rig.post(&request).await;

    rig.enqueue_sse(vec![
        sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0, "content_block":
                {"type": "text", "text": ""}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "text_delta", "text": "dangling "}})
            .to_string(),
        ),
        sse(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "text_delta", "text": "["}})
            .to_string(),
        ),
        sse("message_stop", &json!({"type": "message_stop"}).to_string()),
    ])
    .await;

    let reply = rig.post(&request).await;
    let texts = delta_texts(&reply);
    assert_eq!(texts.concat(), "dangling [", "a held tail was lost");
    let flush = reply
        .find("\"text\":\"[\"")
        .expect("the held bracket was not flushed");
    let stop = reply
        .rfind("event: message_stop")
        .expect("a message_stop event");
    assert!(
        flush < stop,
        "the held tail must be flushed before message_stop: {reply}"
    );
}

#[tokio::test]
async fn cache_control_system_tools_and_stop_sequences_stay_valid_json() {
    let rig = Rig::start("cache", "[plugins.placeholders]\nterms = [\"Falcon\"]\n").await;
    let secret = fake_secret();
    let body = json!({
        "model": "claude-sonnet-4-5",
        "system": [
            {"type": "text", "text": format!("system secret {secret}"),
             "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "plain system"}
        ],
        "tools": [{
            "name": "lookup",
            "description": format!("use {secret}"),
            "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}},
            "cache_control": {"type": "ephemeral"}
        }],
        "stop_sequences": [format!("STOP {secret}"), "Falcon"],
        "messages": [{"role": "user", "content": "hello"}]
    })
    .to_string();

    let _ = rig.post(&body).await;
    let sent = rig.seen(0);
    assert!(!sent.body.contains(&secret), "{}", sent.body);
    let parsed: Value = serde_json::from_str(&sent.body).unwrap();
    let placeholder = first_placeholder(&sent.body);
    assert_eq!(parsed["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(parsed["system"][1]["text"], "plain system");
    assert_eq!(parsed["tools"][0]["name"], "lookup");
    assert_eq!(parsed["tools"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(parsed["tools"][0]["input_schema"]["type"], "object");
    assert!(parsed["system"][0]["text"]
        .as_str()
        .unwrap()
        .contains(&placeholder));
    assert!(parsed["tools"][0]["description"]
        .as_str()
        .unwrap()
        .contains(&placeholder));
    assert!(parsed["stop_sequences"][0]
        .as_str()
        .unwrap()
        .contains(&placeholder));
    assert!(parsed["stop_sequences"][1]
        .as_str()
        .unwrap()
        .contains("[[REIKI_TERM_"));

    let string_system = json!({
        "system": format!("system secret {secret}"),
        "messages": [{"role": "user", "content": "hello"}]
    })
    .to_string();
    let _ = rig.post(&string_system).await;
    let sent = rig.seen(1);
    let parsed: Value = serde_json::from_str(&sent.body).unwrap();
    assert!(!sent.body.contains(&secret), "{}", sent.body);
    assert!(parsed["system"].as_str().unwrap().contains(&placeholder));
}

#[tokio::test]
async fn image_and_document_payloads_pass_through_untouched() {
    let rig = Rig::start("opaque", "[plugins.placeholders]\nterms = [\"Falcon\"]\n").await;
    // Valid base64 that happens to contain credential-shaped runs.
    let image = format!("iVBORw0KGgo/{}0123456789ABCDEF", "AKIA");
    let document = format!("JVBERi0xLjQK/{}0123456789ABCDEF", "AKIA");
    let body = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": image
            }},
            {"type": "document", "source": {
                "type": "base64", "media_type": "application/pdf", "data": document
            }}
        ]}]
    })
    .to_string();

    let _ = rig.post(&body).await;
    let sent = rig.seen(0);
    assert_eq!(sent.body, body, "an opaque binary block was rewritten");
    let parsed: Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!(parsed["messages"][0]["content"][0]["source"]["data"], image);
    assert_eq!(
        parsed["messages"][0]["content"][1]["source"]["data"],
        document
    );
    assert!(!sent.body.contains("[[REIKI_"));
}
