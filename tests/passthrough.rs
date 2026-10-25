use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{router, App};

/// Echoes the headers it was sent, or refuses with a 429 on `/limited`.
async fn mock(req: Request) -> Response {
    if req.uri().path().ends_with("/limited") {
        return Response::builder()
            .status(429)
            .header("retry-after", "7")
            .body(Body::from("{\"error\":\"FreeUsageLimitError\"}"))
            .unwrap();
    }
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_owned()
    };
    let echo = format!(
        "{}|{}|{}|{}|{}",
        header("authorization"),
        header("user-agent"),
        header("x-opencode-client"),
        header("x-api-key"),
        req.uri()
    );
    Response::builder().body(Body::from(echo)).unwrap()
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn relay() -> String {
    let upstream = serve(Router::new().fallback(mock)).await;
    let text = format!(
        "[upstream.direct]\nbase_url = \"{upstream}/zen/v1\"\npassthrough = true\n\
         [upstream.other]\nbase_url = \"{upstream}/o\"\n\
         [keys.k]\nupstream = \"other\"\nfile = \"/unused/k\"\n\
         [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        reiki::auth::sha256_hex("never-sent"),
    );
    let config = Config::parse(&text).unwrap();
    let app = App::with_plugins(
        config,
        [("k".to_owned(), reiki::proxy::Secret::new("reiki-held-key"))]
            .into_iter()
            .collect(),
        Policy::default(),
        Duration::from_secs(30),
        PluginChain::default(),
    );
    serve(router(app)).await
}

#[tokio::test]
async fn the_clients_own_credentials_and_headers_reach_the_upstream_untouched() {
    let relay = relay().await;

    let body = reqwest::Client::new()
        .post(format!("{relay}/direct/chat/completions?x=1"))
        .header("authorization", "Bearer the-clients-own-key")
        .header("user-agent", "opencode/1.18.35")
        .header("x-opencode-client", "cli")
        .header("x-api-key", "also-the-clients")
        .body("{}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(
        body,
        "Bearer the-clients-own-key|opencode/1.18.35|cli|also-the-clients|/zen/v1/chat/completions?x=1"
    );
}

#[tokio::test]
async fn no_reiki_token_is_needed_and_none_of_reikis_keys_are_used() {
    let relay = relay().await;

    let status = reqwest::Client::new()
        .get(format!("{relay}/direct/models"))
        .send()
        .await
        .unwrap();
    let body = status.text().await.unwrap();

    assert!(body.starts_with("-|"), "no credential was invented: {body}");
    assert!(!body.contains("reiki-held-key"));
}

#[tokio::test]
async fn an_upstream_refusal_is_returned_as_it_came_without_failover() {
    let relay = relay().await;

    let response = reqwest::Client::new()
        .post(format!("{relay}/direct/limited"))
        .header("authorization", "Bearer the-clients-own-key")
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(response.headers()["retry-after"], "7");
    assert_eq!(
        response.text().await.unwrap(),
        "{\"error\":\"FreeUsageLimitError\"}"
    );
}

#[tokio::test]
async fn other_upstreams_still_require_a_reiki_token() {
    let relay = relay().await;

    let status = reqwest::Client::new()
        .post(format!("{relay}/other/chat/completions"))
        .header("authorization", "Bearer the-clients-own-key")
        .body("{}")
        .send()
        .await
        .unwrap()
        .status();

    assert_eq!(status.as_u16(), 401);
}

#[test]
fn a_passthrough_upstream_cannot_have_keys_or_a_fallback() {
    let base = "[upstream.direct]\nbase_url = \"https://a.example/v1\"\npassthrough = true\n";
    let hash = reiki::auth::sha256_hex("t");
    let with_key = format!(
        "{base}[keys.k]\nupstream = \"direct\"\nfile = \"/unused/k\"\n\
         [streams.s]\ntoken_sha256 = \"{hash}\"\nchain = [\"k\"]\n"
    );
    let error = Config::parse(&with_key).unwrap_err().to_string();
    assert!(error.contains("passthrough"), "{error}");

    let with_fallback = format!("{base}[upstream.direct.fallback]\nupstream = \"direct\"\n");
    assert!(Config::parse(&with_fallback).is_err());
}
