//! Per-upstream transport options: `forward_accept_encoding` and `http2`.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{router, App, Secret};

const CLIENT_TOKEN: &str = "client-token";
const CLIENT_ENCODING: &str = "gzip, deflate, br, zstd";

#[derive(Default)]
struct Seen(Mutex<Vec<(String, String)>>);

impl Seen {
    /// The `(key, accept-encoding)` of every call, in order.
    fn calls(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A valid gzip stream holding `data` in one stored (uncompressed) block.
fn gzip(data: &[u8]) -> Vec<u8> {
    let len = u16::try_from(data.len()).unwrap();
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1];
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&u32::from(len).to_le_bytes());
    out
}

async fn mock(State(seen): State<Arc<Seen>>, req: Request) -> Response {
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let key = header("authorization")
        .trim_start_matches("Bearer ")
        .to_owned();
    let encoding = header("accept-encoding");
    seen.0.lock().unwrap().push((key.clone(), encoding.clone()));

    // Only compress when the caller offered it, as a real server does.
    let compress = encoding.contains("gzip");
    let reply = |status: StatusCode, text: &str| {
        let mut builder = Response::builder().status(status);
        let body = if compress {
            builder = builder.header("content-encoding", "gzip");
            gzip(text.as_bytes())
        } else {
            text.as_bytes().to_vec()
        };
        builder.body(Body::from(body)).unwrap()
    };
    if key == "key-a" {
        // Only the text marks this 400 as an exhausted key, so it must be readable.
        return reply(
            StatusCode::BAD_REQUEST,
            "{\"error\":\"Your credit balance is too low\"}",
        );
    }
    reply(StatusCode::OK, "{\"ok\":true}")
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// A relay in front of the mock. `options` goes in the upstream table.
async fn relay(options: &str) -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let mock_url = serve(Router::new().fallback(mock).with_state(Arc::clone(&seen))).await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n{options}\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [keys.b]\nupstream = \"zen\"\nfile = \"/unused/b\"\n\
         [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"a\", \"b\"]\n",
        reiki::auth::sha256_hex(CLIENT_TOKEN)
    );
    let config = Config::parse(&text).unwrap();
    let keys = [("a", "key-a"), ("b", "key-b")]
        .into_iter()
        .map(|(name, secret)| (name.to_owned(), Secret::new(secret)))
        .collect();
    let app = App::new(config, keys, Policy::default(), PluginChain::default());
    (serve(router(app)).await, seen)
}

/// A client that never decodes, so what the relay sends is what the test sees.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .build()
        .unwrap()
}

async fn call(base: &str) -> (StatusCode, Option<String>, String) {
    let response = client()
        .post(format!("{base}/zen/chat/completions"))
        .bearer_auth(CLIENT_TOKEN)
        .header("accept-encoding", CLIENT_ENCODING)
        .body("{}")
        .send()
        .await
        .unwrap();
    let status = response.status();
    let encoding = response
        .headers()
        .get("content-encoding")
        .map(|v| v.to_str().unwrap().to_owned());
    (status, encoding, response.text().await.unwrap())
}

#[tokio::test]
async fn the_clients_accept_encoding_is_replaced_by_identity_by_default() {
    let (base, seen) = relay("").await;
    let (status, encoding, body) = call(&base).await;
    // Key a answers 400 in plain text; "credit balance is too low" fails over to key b.
    assert_eq!(status, 200, "{body}");
    assert_eq!(encoding, None);
    assert_eq!(body, "{\"ok\":true}");
    assert_eq!(
        seen.calls(),
        vec![
            ("key-a".to_owned(), "identity".to_owned()),
            ("key-b".to_owned(), "identity".to_owned()),
        ]
    );
}

#[tokio::test]
async fn forwarding_the_accept_encoding_still_reads_compressed_errors_and_fails_over() {
    let (base, seen) = relay("forward_accept_encoding = true").await;
    let (status, encoding, body) = call(&base).await;
    // The 400 from key a arrives gzipped; only its text sends the request on to key b.
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "{\"ok\":true}");
    // The relay decodes, so the client gets plain bytes with no stale encoding header.
    assert_eq!(encoding, None);
    assert_eq!(
        seen.calls(),
        vec![
            ("key-a".to_owned(), CLIENT_ENCODING.to_owned()),
            ("key-b".to_owned(), CLIENT_ENCODING.to_owned()),
        ]
    );
}

#[tokio::test]
async fn forwarding_with_no_client_encoding_asks_for_identity() {
    let (base, seen) = relay("forward_accept_encoding = true").await;
    let response = client()
        .post(format!("{base}/zen/chat/completions"))
        .bearer_auth(CLIENT_TOKEN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(seen.calls().iter().all(|(_, enc)| enc == "identity"));
}

#[tokio::test]
async fn an_upstream_with_http2_enabled_still_works_over_plain_http() {
    // Without TLS there is nothing to negotiate, so the relay falls back to HTTP/1.1.
    let (base, seen) = relay("http2 = true").await;
    let (status, _, body) = call(&base).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(seen.calls().len(), 2);
}
