#![cfg(all(feature = "plugin-placeholders", feature = "plugin-requestlog", unix))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{builtin, PluginChain};
use reiki::proxy::{router, App, Secret};

const CLIENT_TOKEN: &str = "client-token";
const UPSTREAM_KEY: &str = "upstream-key-do-not-log";

#[derive(Default)]
struct Seen(Mutex<Vec<String>>);

/// Records what the upstream received, then answers according to the path.
async fn upstream(State(seen): State<Arc<Seen>>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let body = String::from_utf8(
        to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    seen.0.lock().unwrap().push(body.clone());

    if path.ends_with("/stream") {
        // The model writes the placeholder it was given, one event ending mid-placeholder.
        let start = body.find("[[REIKI_TERM_").unwrap();
        let end = body[start..].find("]]").unwrap() + start + 2;
        let (first, second) = body[start..end].split_at(9);
        let event = |text: &str| {
            let json = serde_json::json!({"choices": [{"delta": {"content": text}}]});
            format!("data: {json}\n\n")
        };
        let events = [
            event(&format!("The status of {first}")),
            event(&format!("{second} is green.")),
            "data: [DONE]\n\n".to_owned(),
        ];
        let stream = futures_util::stream::unfold(events.into_iter(), |mut it| async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            it.next()
                .map(|e| (Ok::<_, std::io::Error>(axum::body::Bytes::from(e)), it))
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"echo\":{}}}",
            serde_json::to_string(&body).unwrap()
        )))
        .unwrap()
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
    seen: Arc<Seen>,
    dir: PathBuf,
}

impl Rig {
    async fn start(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("reiki-ph-it-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let seen = Arc::new(Seen::default());
        let mock = serve(
            Router::new()
                .fallback(upstream)
                .with_state(Arc::clone(&seen)),
        )
        .await;
        let text = format!(
            "[upstream.zen]\nbase_url = \"{mock}/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
             [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
             [plugins.placeholders]\nterms = [\"Falcon\"]\n\
             [plugins.requestlog]\nfile = \"{}/req.jsonl\"\nresponse = \"body\"\n",
            reiki::auth::sha256_hex(CLIENT_TOKEN),
            dir.display()
        );
        let config = Config::parse(&text).unwrap();
        let chain = PluginChain::build(&config.plugins, &builtin()).unwrap();
        assert_eq!(
            chain.names().collect::<Vec<_>>(),
            ["placeholders", "requestlog"],
            "masking must come before logging"
        );
        let keys = [("a".to_owned(), Secret::new(UPSTREAM_KEY))]
            .into_iter()
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
            seen,
            dir,
        }
    }

    async fn post(&self, path: &str, body: &str) -> String {
        reqwest::Client::new()
            .post(format!("{}{path}", self.relay))
            .bearer_auth(CLIENT_TOKEN)
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    fn log(&self) -> String {
        let path: &Path = &self.dir.join("req.jsonl");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            if text.lines().count() >= 2 || Instant::now() > deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Assembled at runtime so no token-shaped literal sits in the source.
fn fake_token() -> String {
    ["ghp_", "0123456789abcdef0123456789abcdef0123"].concat()
}

#[tokio::test]
async fn the_upstream_and_the_log_see_placeholders_and_the_client_gets_the_originals() {
    let rig = Rig::start("echo").await;
    let token = fake_token();
    let sent = format!("{{\"q\":\"Is Falcon late? use {token}\"}}");

    let reply = rig.post("/zen/chat/completions", &sent).await;

    // The upstream never received either value.
    let upstream_body = rig.seen.0.lock().unwrap()[0].clone();
    assert!(
        !upstream_body.contains("Falcon") && !upstream_body.contains(&token),
        "{upstream_body}"
    );
    assert!(upstream_body.contains("[[REIKI_TERM_") && upstream_body.contains("[[REIKI_SECRET_"));

    // The client got them back, byte for byte.
    assert!(
        reply.contains("Is Falcon late?") && reply.contains(&token),
        "{reply}"
    );
    assert!(!reply.contains("REIKI"), "{reply}");

    // The log holds the masked request and the masked response.
    let log = rig.log();
    assert!(
        !log.contains("Falcon") && !log.contains(&token),
        "sensitive value in the log: {log}"
    );
    assert_eq!(
        log.matches("[[REIKI_TERM_").count(),
        2,
        "request and response: {log}"
    );
    let _ = std::fs::remove_dir_all(&rig.dir);
}

#[tokio::test]
async fn a_placeholder_split_across_streamed_events_is_restored() {
    let rig = Rig::start("stream").await;

    let reply = rig
        .post("/zen/stream", "{\"q\":\"How is Falcon doing?\"}")
        .await;

    let text: String = reply
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(text, "The status of Falcon is green.");
    assert!(reply.ends_with("data: [DONE]\n\n"), "{reply}");

    let log = rig.log();
    assert!(!log.contains("Falcon"), "sensitive value in the log: {log}");
    let _ = std::fs::remove_dir_all(&rig.dir);
}
