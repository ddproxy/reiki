#![cfg(all(feature = "plugin-requestlog", unix))]

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{builtin, PluginChain};
use reiki::proxy::{router, App, Secret};
use serde_json::Value;

const CLIENT_TOKEN: &str = "client-token-do-not-log";
const KEY_A: &str = "upstream-key-a-do-not-log";
const KEY_B: &str = "upstream-key-b-do-not-log";

async fn upstream(req: Request) -> Response {
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if auth.contains(KEY_A) {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("retry-after", "60")
            .body(Body::from("Rate limit exceeded"))
            .unwrap();
    }
    if req.uri().path().ends_with("/limited") {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("retry-after", "60")
            .body(Body::from("Rate limit exceeded"))
            .unwrap();
    }
    if req.uri().path().ends_with("/stream") {
        let chunks = ["data: one\n\n", "data: two\n\n", "data: [DONE]\n\n"];
        let stream = futures_util::stream::unfold(chunks.into_iter(), |mut it| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            it.next()
                .map(|c| (Ok::<_, std::io::Error>(axum::body::Bytes::from(c)), it))
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    Response::new(Body::from("{\"ok\":true}"))
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

struct Rig {
    relay: String,
    client: reqwest::Client,
    dir: PathBuf,
}

impl Rig {
    async fn start(tag: &str, plugin_table: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("reiki-rl-it-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let plugin_table = plugin_table.replace("{dir}", &dir.display().to_string());
        let mock = serve(Router::new().fallback(upstream)).await;
        let text = format!(
            "[upstream.zen]\nbase_url = \"{mock}/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
             [keys.b]\nupstream = \"zen\"\nfile = \"/unused/b\"\n\
             [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"a\", \"b\"]\n\
             [plugins.requestlog]\n{plugin_table}\n",
            reiki::auth::sha256_hex(CLIENT_TOKEN)
        );
        let config = Config::parse(&text).unwrap();
        let chain = PluginChain::build(&config.plugins, &builtin()).unwrap();
        let keys = [("a", KEY_A), ("b", KEY_B)]
            .into_iter()
            .map(|(name, secret)| (name.to_owned(), Secret::new(secret)))
            .collect();
        let app = App::with_plugins(
            config,
            keys,
            Policy::default(),
            Duration::from_secs(120),
            chain,
        );
        Self {
            relay: serve(router(app)).await,
            client: reqwest::Client::new(),
            dir,
        }
    }

    fn post(&self, path: &str, body: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.relay))
            .bearer_auth(CLIENT_TOKEN)
            .header("user-agent", "opencode/1.18.35")
            .body(body.to_owned())
    }
}

fn records_when(path: &Path, want: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let records: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if records.len() >= want || Instant::now() > deadline {
            return records;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn a_failover_request_is_logged_once_with_its_final_response() {
    let rig = Rig::start(
        "failover",
        "file = \"{dir}/req.jsonl\"\nheaders = [\"user-agent\"]",
    )
    .await;
    let path = rig.dir.join("req.jsonl");

    let body = r#"{"model":"big-pickle","messages":[{"role":"user","content":"hello there"}]}"#;
    let response = rig
        .post("/zen/chat/completions", body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "{\"ok\":true}");

    let records = records_when(&path, 2);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["event"], "request");
    assert_eq!(records[0]["stream"], "ct");
    assert_eq!(records[0]["upstream"], "zen");
    assert_eq!(records[0]["method"], "POST");
    assert_eq!(records[0]["path"], "/zen/chat/completions");
    assert_eq!(records[0]["headers"]["user-agent"], "opencode/1.18.35");
    assert_eq!(records[0]["body"]["messages"][0]["content"], "hello there");
    assert_eq!(records[1]["event"], "response");
    assert_eq!(records[1]["id"], records[0]["id"]);
    assert_eq!(records[1]["status"], 200);
    // The response record names the key that served it and each key tried, by its config name.
    assert_eq!(records[1]["key"], "b");
    assert_eq!(
        records[1]["attempts"],
        serde_json::json!([
            {"key": "a", "status": 429},
            {"key": "b", "status": 200},
        ])
    );
    // The default mode records no response body, not even for a failed attempt.
    assert!(records[1].get("body").is_none());
    assert!(records[1]["attempts"][0].get("body").is_none());

    let text = std::fs::read_to_string(&path).unwrap();
    for secret in [CLIENT_TOKEN, KEY_A, KEY_B] {
        assert!(!text.contains(secret), "{secret} leaked into the log");
    }
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn a_streamed_response_is_counted_and_closed_after_the_last_chunk() {
    let rig = Rig::start("streamed", "file = \"{dir}/req.jsonl\"").await;
    let path = rig.dir.join("req.jsonl");

    let response = rig.post("/zen/stream", "{}").send().await.unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("[DONE]"));

    let records = records_when(&path, 2);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["event"], "response");
    assert_eq!(records[1]["bytes"], text.len());
    assert!(records[1]["duration_ms"].as_u64().unwrap() >= 40);
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn an_answer_without_an_upstream_response_is_logged_with_a_null_status() {
    let rig = Rig::start("exhausted", "file = \"{dir}/req.jsonl\"").await;
    let path = rig.dir.join("req.jsonl");

    let response = rig.post("/zen/limited", "{}").send().await.unwrap();
    assert_eq!(response.status(), 503);

    let records = records_when(&path, 2);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["event"], "response");
    assert_eq!(records[1]["id"], records[0]["id"]);
    assert!(records[1]["status"].is_null(), "{}", records[1]);
    assert_eq!(records[1]["bytes"], 0);
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn the_live_stream_carries_requests_without_credentials() {
    let rig = Rig::start("socket", "socket = \"{dir}/req.sock\"\nresponse = \"none\"").await;
    let sock = rig.dir.join("req.sock");

    let client = UnixStream::connect(&sock).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let response = rig
        .post("/zen/chat/completions", r#"{"model":"live"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).unwrap();
    let record: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(record["body"]["model"], "live");
    for secret in [CLIENT_TOKEN, KEY_A, KEY_B] {
        assert!(!line.contains(secret), "{secret} leaked into the stream");
    }
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn an_invalid_plugin_table_stops_the_chain_from_building() {
    let mock = serve(Router::new().fallback(upstream)).await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"{mock}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [plugins.requestlog]\nheaders = [\"authorization\"]\nfile = \"/tmp/x.jsonl\"\n",
        reiki::auth::sha256_hex(CLIENT_TOKEN)
    );
    let config = Config::parse(&text).unwrap();
    assert!(PluginChain::build(&config.plugins, &builtin()).is_err());
}

#[tokio::test]
async fn body_mode_records_the_response_body_and_never_a_key() {
    let rig = Rig::start(
        "respbody",
        "file = \"{dir}/req.jsonl\"\nresponse = \"body\"",
    )
    .await;
    let path = rig.dir.join("req.jsonl");

    let response = rig
        .post("/zen/chat/completions", "{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.text().await.unwrap();

    let records = records_when(&path, 2);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["body"]["ok"], true);
    assert_eq!(records[1]["key"], "b");
    // The failed attempt keeps the upstream's own error text.
    assert_eq!(records[1]["attempts"][0]["body"], "Rate limit exceeded");
    assert!(records[1]["attempts"][1].get("body").is_none());

    let text = std::fs::read_to_string(&path).unwrap();
    for secret in [CLIENT_TOKEN, KEY_A, KEY_B] {
        assert!(!text.contains(secret), "{secret} leaked into the log");
    }
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn a_streamed_response_body_is_kept_up_to_the_cap_and_marked_cut() {
    let rig = Rig::start(
        "respcap",
        "file = \"{dir}/req.jsonl\"\nresponse = \"body\"\nmax_body_bytes = 20",
    )
    .await;
    let path = rig.dir.join("req.jsonl");

    let response = rig.post("/zen/stream", "{}").send().await.unwrap();
    let text = response.text().await.unwrap();
    assert!(text.len() > 20);

    let records = records_when(&path, 2);
    assert_eq!(records.len(), 2, "{records:?}");
    let body = records[1]["body"].as_str().unwrap();
    assert!(body.starts_with("data: one"), "{body}");
    assert!(body.len() <= 20, "{body}");
    assert_eq!(records[1]["body_truncated"], true);
    // The byte count still covers the whole response.
    assert_eq!(records[1]["bytes"], text.len());
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn an_exhausted_chain_logs_every_attempt_and_no_serving_key() {
    let rig = Rig::start("attempts", "file = \"{dir}/req.jsonl\"").await;
    let path = rig.dir.join("req.jsonl");

    let response = rig.post("/zen/limited", "{}").send().await.unwrap();
    assert_eq!(response.status(), 503);

    let records = records_when(&path, 2);
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(records[1]["key"].is_null(), "{}", records[1]);
    assert_eq!(
        records[1]["attempts"],
        serde_json::json!([
            {"key": "a", "status": 429},
            {"key": "b", "status": 429},
        ])
    );
    let _ = std::fs::remove_dir_all(&rig.dir);
}
