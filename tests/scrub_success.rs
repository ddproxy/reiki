//! An upstream that echoes the key it was sent must not hand it to the client, with or without
//! plugins in the chain, and a response with a content length keeps a correct one.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::{builtin, PluginChain};
use reiki::proxy::{load_keys, router, App};

const TOKEN: &str = "stream-token";
const KEY: &str = "upstream-secret-key-1234";

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn echo(headers: HeaderMap) -> impl IntoResponse {
    let sent = headers["authorization"].to_str().unwrap().to_owned();
    let body = format!("{{\"echo\":\"{sent}\"}}");
    let mut out = HeaderMap::new();
    out.insert("x-echo", HeaderValue::from_str(&sent).unwrap());
    (out, body)
}

async fn relay(plugins: &str, tag: &str) -> (String, reqwest::Client) {
    let dir = std::env::temp_dir().join(format!("reiki-scrub-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("key"), format!("{KEY}\n")).unwrap();
    let upstream = serve(Router::new().fallback(any(echo))).await;
    let text = format!(
        "listen = \"127.0.0.1:0\"\n[upstream.zen]\nbase_url = \"{upstream}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"{key}\"\n\
         [streams.s]\ntoken_sha256 = \"{hash}\"\nchain = [\"a\"]\n{plugins}",
        key = dir.join("key").display(),
        hash = reiki::auth::sha256_hex(TOKEN),
    );
    let config = Config::parse(&text).unwrap();
    let keys = load_keys(&config).unwrap();
    let chain = PluginChain::build(&config.plugins, &builtin()).unwrap();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(5),
        chain,
    );
    (
        serve(router(Arc::clone(&app))).await,
        reqwest::Client::new(),
    )
}

async fn call(plugins: &str, tag: &str) -> (reqwest::header::HeaderMap, String) {
    let (url, client) = relay(plugins, tag).await;
    let response = client
        .post(format!("{url}/zen/chat"))
        .bearer_auth(TOKEN)
        .body("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers().clone();
    (headers, response.text().await.unwrap())
}

#[tokio::test]
async fn an_echoed_key_is_scrubbed_with_no_plugins() {
    let (headers, body) = call("", "none").await;
    assert!(!body.contains(KEY), "{body}");
    assert!(!headers["x-echo"].to_str().unwrap().contains(KEY));
    assert!(
        body.starts_with("{\"echo\":\"["),
        "the key was blanked in place: {body}"
    );
}

#[tokio::test]
async fn an_echoed_key_is_scrubbed_before_a_plugin_sees_it() {
    let (headers, body) = call("[plugins.phi]\nmode = \"report\"\n", "plugin").await;
    assert!(!body.contains(KEY), "{body}");
    assert!(!headers["x-echo"].to_str().unwrap().contains(KEY));
}

#[tokio::test]
async fn a_response_without_the_key_is_unchanged_and_keeps_its_length() {
    let dir = std::env::temp_dir().join(format!("reiki-scrub-{}-clean", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("key"), format!("{KEY}\n")).unwrap();
    let upstream = serve(Router::new().fallback(any(|| async { "plain body" }))).await;
    let text = format!(
        "listen = \"127.0.0.1:0\"\n[upstream.zen]\nbase_url = \"{upstream}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"{key}\"\n\
         [streams.s]\ntoken_sha256 = \"{hash}\"\nchain = [\"a\"]\n",
        key = dir.join("key").display(),
        hash = reiki::auth::sha256_hex(TOKEN),
    );
    let config = Config::parse(&text).unwrap();
    let keys = load_keys(&config).unwrap();
    let app = App::with_plugins(
        config,
        keys,
        Policy::default(),
        Duration::from_secs(5),
        PluginChain::default(),
    );
    let url = serve(router(app)).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/zen/chat"))
        .bearer_auth(TOKEN)
        .body("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-length"], "10");
    assert_eq!(response.text().await.unwrap(), "plain body");
}
