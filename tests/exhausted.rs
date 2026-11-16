use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, router, App};
use tokio::net::TcpListener;

const LIMIT_BODY: &str = r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"Free usage limit reached. Free models work in OpenCode (https://opencode.ai); subscribe to OpenCode Go to keep going: https://opencode.ai/go"}}"#;

/// Answers every request with a 429 and counts them.
async fn limited(hits: Arc<AtomicUsize>) -> u16 {
    limited_with(hits, None).await
}

/// The same, with a `Retry-After` header when one is given.
async fn limited_with(hits: Arc<AtomicUsize>, retry_after: Option<&'static str>) -> u16 {
    let app = Router::new().fallback(move |_: Request| {
        let hits = Arc::clone(&hits);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            let mut response = Response::new(axum::body::Body::from(LIMIT_BODY));
            *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            if let Some(value) = retry_after {
                response
                    .headers_mut()
                    .insert("retry-after", value.parse().unwrap());
            }
            response
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

#[tokio::test]
async fn a_chain_of_limited_keys_answers_promptly() {
    let hits = Arc::new(AtomicUsize::new(0));
    let port = limited(Arc::clone(&hits)).await;
    let dir = std::env::temp_dir().join(format!("reiki-exhausted-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut keys = String::new();
    for n in 1..=4 {
        std::fs::write(dir.join(format!("k{n}")), format!("upstream-key-{n}")).unwrap();
        keys.push_str(&format!(
            "[keys.k{n}]\nupstream = \"api\"\nfile = \"{}\"\n",
            dir.join(format!("k{n}")).display()
        ));
    }
    let text = format!(
        "[upstream.api]\nbase_url = \"http://127.0.0.1:{port}/v1\"\n{keys}\n[streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k1\", \"k2\", \"k3\", \"k4\"]\n",
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        Config::parse(&text).unwrap(),
        load_keys(&config).unwrap(),
        Policy::default(),
        PluginChain::default(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });

    let started = Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        reqwest::Client::new()
            .post(format!("http://{addr}/api/chat/completions"))
            .bearer_auth("tok")
            .body("{}")
            .send(),
    )
    .await
    .expect("the request hung")
    .unwrap();
    let status = response.status().as_u16();
    let _ = response.text().await;
    eprintln!(
        "status={status} after {:?}, {} upstream hits",
        started.elapsed(),
        hits.load(Ordering::SeqCst)
    );
    assert_eq!(hits.load(Ordering::SeqCst), 4, "each key is tried once");
    assert!(status == 429 || status == 503, "status {status}");
}

/// A quota 429 that says when to come back is cooled for that long, not for the 5 hour default.
#[tokio::test]
async fn a_quota_429_with_retry_after_cools_the_key_for_that_long() {
    let hits = Arc::new(AtomicUsize::new(0));
    let port = limited_with(Arc::clone(&hits), Some("90")).await;
    let dir = std::env::temp_dir().join(format!("reiki-quota-hint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k1"), "upstream-key-1").unwrap();
    let text = format!(
        "[upstream.api]\nbase_url = \"http://127.0.0.1:{port}/v1\"\n\
         [keys.k1]\nupstream = \"api\"\nfile = \"{}\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k1\"]\n",
        dir.join("k1").display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        Config::parse(&text).unwrap(),
        load_keys(&config).unwrap(),
        Policy::default(),
        PluginChain::default(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });

    let client = reqwest::Client::new();
    let send = || {
        client
            .post(format!("http://{addr}/api/chat/completions"))
            .bearer_auth("tok")
            .body("{}")
            .send()
    };
    let _ = send().await.unwrap().text().await;
    // The key is now out of rotation, so the second request is refused locally and says how
    // long until the key is back.
    let second = send().await.unwrap();
    assert_eq!(second.status().as_u16(), 503);
    let wait: u64 = second.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (60..=90).contains(&wait),
        "waits {wait}s, not the 5 hour default"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the cooled key was not tried again"
    );
}
