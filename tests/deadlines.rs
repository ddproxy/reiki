//! Response body idle and total timeouts, on both buffered (decoded) and streamed bodies.

use std::sync::Arc;
use std::time::Duration;

use async_compression::tokio::write::GzipEncoder;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{router, App, Secret};
use tokio::io::AsyncWriteExt;

const CLIENT_TOKEN: &str = "client-token";

/// A valid gzip stream that expands to 64 KiB, so decoding is observable.
async fn gzipped() -> Vec<u8> {
    let mut encoder = GzipEncoder::new(Vec::new());
    encoder.write_all(&vec![b'a'; 64 * 1024]).await.unwrap();
    encoder.shutdown().await.unwrap();
    encoder.into_inner()
}

/// Streams `chunks` with `gap` between them, then sleeps `tail` before ending. The sleep
/// without a following chunk is what an idle timeout has to notice.
fn paced(chunks: Vec<Vec<u8>>, gap: Duration, tail: Option<Duration>) -> Body {
    let stream = futures_util::stream::unfold(
        (chunks.into_iter(), 0usize, tail),
        move |(mut chunks, index, tail)| async move {
            if index > 0 {
                tokio::time::sleep(gap).await;
            }
            match chunks.next() {
                Some(chunk) => Some((
                    Ok::<_, std::io::Error>(Bytes::from(chunk)),
                    (chunks, index + 1, tail),
                )),
                None => {
                    if let Some(tail) = tail {
                        tokio::time::sleep(tail).await;
                    }
                    None
                }
            }
        },
    );
    Body::from_stream(stream)
}

async fn mock(State(gzip): State<Arc<Vec<u8>>>, req: Request<Body>) -> Response {
    let path = req.uri().path().to_owned();
    if path.ends_with("/slow-gzip") {
        // Everything but the gzip trailer arrives, then the stream stalls without ending it,
        // so the decoder cannot finish and the idle timer is the only way out.
        let partial = gzip[..gzip.len() - 8].to_vec();
        Response::builder()
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(paced(
                vec![partial],
                Duration::ZERO,
                Some(Duration::from_secs(5)),
            ))
            .unwrap()
    } else if path.ends_with("/slow-sse") {
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(paced(
                vec![b"data: one\n\n".to_vec()],
                Duration::ZERO,
                Some(Duration::from_secs(5)),
            ))
            .unwrap()
    } else {
        // Four pieces over about 1.6 s, so the idle timer never fires but the total does.
        let size = gzip.len().div_ceil(4).max(1);
        let chunks = gzip.chunks(size).map(<[u8]>::to_vec).collect();
        Response::builder()
            .header("content-type", "application/json")
            .header("content-encoding", "gzip")
            .body(paced(chunks, Duration::from_millis(400), None))
            .unwrap()
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

struct Rig {
    relay: String,
    client: reqwest::Client,
}

async fn rig(upstream_extra: &str) -> Rig {
    let gzip = Arc::new(gzipped().await);
    let mock_url = serve(Router::new().fallback(mock).with_state(Arc::clone(&gzip))).await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n{upstream_extra}\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
         [streams.ct]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n",
        reiki::auth::sha256_hex(CLIENT_TOKEN),
    );
    let config = Config::parse(&text).unwrap();
    let keys = [("a".to_owned(), Secret::new("up-key"))]
        .into_iter()
        .collect();
    let app = App::new(config, keys, Policy::default(), PluginChain::default());
    Rig {
        relay: serve(router(app)).await,
        client: reqwest::Client::new(),
    }
}

impl Rig {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.relay))
            .bearer_auth(CLIENT_TOKEN)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_buffered_body_that_stalls_past_the_idle_timeout_is_a_clean_504() {
    let rig = rig("response_idle_timeout_secs = 1").await;
    let response = rig.get("/zen/slow-gzip").await;
    // Buffering the decoded body means the client sees one status, not a truncated 200.
    assert_eq!(response.status(), 504);
    let body = response.text().await.unwrap();
    assert!(body.contains("timed out"), "body was {body}");
}

#[tokio::test]
async fn a_streamed_body_that_stalls_past_the_idle_timeout_ends_with_an_error() {
    let rig = rig("response_idle_timeout_secs = 1").await;
    let response = rig.get("/zen/slow-sse").await;
    assert_eq!(response.status(), 200);
    // SSE is relayed as it arrives; the body ends with a transport error, not a hang.
    assert!(response.bytes().await.is_err());
}

#[tokio::test]
async fn a_body_that_keeps_trickling_past_the_total_timeout_is_a_clean_504() {
    let rig = rig("response_idle_timeout_secs = 10\nresponse_timeout_secs = 1").await;
    let response = rig.get("/zen/total-gzip").await;
    assert_eq!(response.status(), 504);
    let body = response.text().await.unwrap();
    assert!(body.contains("timed out"), "body was {body}");
}
