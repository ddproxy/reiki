use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{router, App, Secret};

const CLIENT_TOKEN: &str = "client-token";

/// Status and body an upstream refuses everything with; `None` answers normally.
type Refusal = Option<(u16, &'static str)>;

#[derive(Default)]
struct Seen(Mutex<Vec<(String, String)>>);

/// Refuses everything for the exhausted upstream and echoes the model from the other.
async fn mock(State((seen, refusal)): State<(Arc<Seen>, Refusal)>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let body = String::from_utf8(
        to_bytes(req.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    seen.0.lock().unwrap().push((path, body.clone()));
    if let Some((status, text)) = refusal {
        return Response::builder()
            .status(status)
            .body(Body::from(text))
            .unwrap();
    }
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(body))
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
    zen: Arc<Seen>,
    other: Arc<Seen>,
}

async fn start(fallback: &str, chain: &str) -> Rig {
    start_refusing(
        fallback,
        chain,
        402,
        "{\"error\":\"Insufficient account funds\"}",
    )
    .await
}

async fn start_refusing(fallback: &str, chain: &str, status: u16, text: &'static str) -> Rig {
    let zen = Arc::new(Seen::default());
    let other = Arc::new(Seen::default());
    let zen_url = serve(
        Router::new()
            .fallback(mock)
            .with_state((Arc::clone(&zen), Some((status, text)))),
    )
    .await;
    let other_url = serve(
        Router::new()
            .fallback(mock)
            .with_state((Arc::clone(&other), None)),
    )
    .await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"{zen_url}/v1\"\n{fallback}\n\
         [upstream.router]\nbase_url = \"{other_url}/api\"\nmodel_aliases = {{ \"mapped\" = \"final-model\" }}\n\
         [keys.z]\nupstream = \"zen\"\nfile = \"/unused/z\"\n\
         [keys.r]\nupstream = \"router\"\nfile = \"/unused/r\"\n\
         [streams.ct]\ntoken_sha256 = \"{}\"\nchain = {chain}\n",
        reiki::auth::sha256_hex(CLIENT_TOKEN),
    );
    let config = Config::parse(&text).unwrap();
    let keys = [
        ("z".to_owned(), Secret::new("zen-key")),
        ("r".to_owned(), Secret::new("router-key")),
    ]
    .into_iter()
    .collect();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(30),
        PluginChain::default(),
    );
    Rig {
        relay: serve(router(app)).await,
        zen,
        other,
    }
}

async fn post(rig: &Rig, body: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{}/zen/chat/completions", rig.relay))
        .bearer_auth(CLIENT_TOKEN)
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

const FALLBACK: &str =
    "[upstream.zen.fallback]\nupstream = \"router\"\nmodel_aliases = { \"*\" = \"mapped\" }";

#[tokio::test]
async fn an_exhausted_upstream_hands_the_request_to_its_fallback() {
    let rig = start(FALLBACK, "[\"z\", \"r\"]").await;

    let (status, body) = post(&rig, "{\"model\":\"big-pickle\",\"stream\":false}").await;

    assert_eq!(status, 200, "{body}");
    // The fallback's mapping and then its own upstream aliases were applied.
    assert_eq!(body, "{\"model\":\"final-model\",\"stream\":false}");
    let landed = rig.other.0.lock().unwrap();
    assert_eq!(
        landed[0].0, "/api/chat/completions",
        "same endpoint, other base url"
    );
    assert_eq!(
        rig.zen.0.lock().unwrap().len(),
        1,
        "tried once before giving up"
    );
}

#[tokio::test]
async fn while_the_first_upstream_cools_down_later_requests_skip_it() {
    let rig = start(FALLBACK, "[\"z\", \"r\"]").await;
    post(&rig, "{\"model\":\"a\"}").await;

    let (status, _) = post(&rig, "{\"model\":\"b\"}").await;

    assert_eq!(status, 200);
    assert_eq!(
        rig.zen.0.lock().unwrap().len(),
        1,
        "no second try on the cooling key"
    );
    assert_eq!(rig.other.0.lock().unwrap().len(), 2);
}

const FREE_TIER: &str = "{\"type\":\"error\",\"error\":{\"type\":\"FreeTierError\",\"message\":\"OpenCode's free tier can only be used from within OpenCode.\"}}";

#[tokio::test]
async fn a_refusal_from_every_key_also_falls_back() {
    let rig = start_refusing(
        FALLBACK,
        "[\"z\", \"r\"]",
        403,
        "{\"error\":\"free tier only\"}",
    )
    .await;

    let (status, body) = post(&rig, "{\"model\":\"a\"}").await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "{\"model\":\"final-model\"}");
}

#[tokio::test]
async fn a_refusal_without_a_fallback_reaches_the_client_unchanged() {
    let rig = start_refusing("", "[\"z\", \"r\"]", 403, FREE_TIER).await;

    let (status, body) = post(&rig, "{\"model\":\"a\"}").await;

    assert_eq!(status, 403);
    assert_eq!(body, FREE_TIER);
}

#[tokio::test]
async fn without_a_fallback_the_client_still_gets_the_exhausted_answer() {
    let rig = start("", "[\"z\", \"r\"]").await;

    let (status, body) = post(&rig, "{\"model\":\"a\"}").await;

    assert_eq!(status, 503, "{body}");
    assert!(rig.other.0.lock().unwrap().is_empty());
}

#[test]
fn a_fallback_needs_a_key_for_the_target_in_every_stream_that_uses_the_source() {
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://a.example/v1\"\n{FALLBACK}\n\
         [upstream.router]\nbase_url = \"https://b.example/v1\"\n\
         [keys.z]\nupstream = \"zen\"\nfile = \"/unused/z\"\n\
         [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"z\"]\n",
        reiki::auth::sha256_hex(CLIENT_TOKEN),
    );
    let error = Config::parse(&text).unwrap_err().to_string();
    assert!(error.contains("no `router` key"), "{error}");
}

#[test]
fn a_fallback_cannot_name_itself_or_an_unknown_upstream() {
    for target in ["zen", "nowhere"] {
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://a.example/v1\"\n\
             [upstream.zen.fallback]\nupstream = \"{target}\"\n\
             [keys.z]\nupstream = \"zen\"\nfile = \"/unused/z\"\n\
             [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"z\"]\n",
            reiki::auth::sha256_hex(CLIENT_TOKEN),
        );
        let error = Config::parse(&text).unwrap_err().to_string();
        assert!(error.contains("invalid fallback"), "{error}");
    }
}
