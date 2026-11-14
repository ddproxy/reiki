use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{PluginChain, Registration};
use reiki::proxy::{router, App, Secret};
use reiki_plugin::{Event, Plugin};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;
use common::serve;

const TOKEN: &str = "client-token";

type Done = Arc<Mutex<Vec<(String, u16)>>>;

static DONE: Mutex<Option<Done>> = Mutex::new(None);

struct Recorder;
impl Plugin for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn on_event(&self, event: &Event<'_>) {
        if let Event::RequestDone {
            upstream, status, ..
        } = event
        {
            if let Some(done) = DONE.lock().unwrap().as_ref() {
                done.lock().unwrap().push(((*upstream).to_owned(), *status));
            }
        }
    }
}

async fn refuse(_: Request) -> Response {
    Response::builder()
        .status(402)
        .body(Body::from("no funds"))
        .unwrap()
}

async fn accept(_: Request) -> Response {
    Response::new(Body::from("ok"))
}

async fn rig(timeout: Duration) -> (String, Done) {
    let zen = serve(Router::new().fallback(refuse)).await;
    let other = serve(Router::new().fallback(accept)).await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"{zen}/v1\"\n[upstream.zen.fallback]\nupstream = \"router\"\n\
         [upstream.router]\nbase_url = \"{other}/api\"\n\
         [keys.z]\nupstream = \"zen\"\nfile = \"/unused/z\"\n\
         [keys.r]\nupstream = \"router\"\nfile = \"/unused/r\"\n\
         [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"z\", \"r\"]\n",
        reiki::auth::sha256_hex(TOKEN),
    );
    let keys = [
        ("z".to_owned(), Secret::new("zen-key")),
        ("r".to_owned(), Secret::new("router-key")),
    ]
    .into_iter()
    .collect();
    let registry = vec![Registration {
        name: "recorder",
        build: |_| Ok(Box::new(Recorder) as Box<dyn Plugin>),
    }];
    let plugins = PluginChain::build(&toml::from_str("[recorder]\n").unwrap(), &registry).unwrap();
    let done: Done = Arc::default();
    *DONE.lock().unwrap() = Some(Arc::clone(&done));
    let app = App::with_plugins(
        Config::parse(&text).unwrap(),
        keys,
        Policy::default(),
        timeout,
        plugins,
    );
    (serve(router(app)).await, done)
}

async fn raw(relay: &str, request: &[u8]) -> String {
    let mut stream = tokio::net::TcpStream::connect(relay.trim_start_matches("http://"))
        .await
        .unwrap();
    stream.write_all(request).await.unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_buf(&mut reply))
        .await
        .expect("no answer")
        .unwrap();
    String::from_utf8_lossy(&reply).into_owned()
}

// Both tests share the recorder slot, so they run as one test.
#[tokio::test]
async fn one_event_reports_the_status_the_client_received() {
    // A fallback answers 200: one event, for the upstream that served it.
    let (relay, done) = rig(Duration::from_secs(30)).await;
    let reply = reqwest::Client::new()
        .post(format!("{relay}/zen/chat"))
        .bearer_auth(TOKEN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    assert_eq!(*done.lock().unwrap(), [("router".to_owned(), 200)]);

    // A path trick is refused with 400, not reported as a disconnect.
    let (relay, done) = rig(Duration::from_secs(30)).await;
    let reply = raw(
        &relay,
        b"POST /zen/%2e%2e/x HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer client-token\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    assert_eq!(*done.lock().unwrap(), [("zen".to_owned(), 400)]);

    // A stalled body is a 408.
    let (relay, done) = rig(Duration::from_millis(300)).await;
    let reply = raw(
        &relay,
        b"POST /zen/x HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer client-token\r\nContent-Length: 100\r\n\r\nab",
    )
    .await;
    assert!(reply.starts_with("HTTP/1.1 408"), "{reply}");
    assert_eq!(*done.lock().unwrap(), [("zen".to_owned(), 408)]);
}
