use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{router, App, Secret};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, UnixListener};
use tokio::time::Instant;

fn python() -> &'static str {
    if which("python3") {
        "python3"
    } else {
        "python"
    }
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn child_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/forward_child.py")
}

static SOCK_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_sock_path() -> PathBuf {
    let n = SOCK_COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("rk-{}-{}.sock", std::process::id(), n));
    p
}

async fn serve_app(app: std::sync::Arc<App>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
    format!("http://{addr}")
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

async fn start_simple_socket_server(
    resp_fn: impl Fn(&Seen) -> (hyper::StatusCode, Vec<(String, String)>, Bytes) + Send + Sync + 'static,
) -> (PathBuf, Arc<Mutex<Vec<Seen>>>) {
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let resp_fn = Arc::new(resp_fn);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let seen_c2 = Arc::clone(&seen_c);
            let resp_fn2 = Arc::clone(&resp_fn);
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let seen_c3 = Arc::clone(&seen_c2);
                    let resp_fn3 = Arc::clone(&resp_fn2);
                    async move {
                        let method = req.method().to_string();
                        let uri = req.uri().clone();
                        let path = uri.path().to_string();
                        let query = uri.query().map(|s| s.to_string());
                        let mut headers = Vec::new();
                        for (name, value) in req.headers().iter() {
                            headers.push((name.to_string(), value.as_bytes().to_vec()));
                        }
                        let (_parts, body) = req.into_parts();
                        let body_bytes = http_body_util::BodyExt::collect(body)
                            .await
                            .unwrap()
                            .to_bytes();
                        let s = Seen {
                            method,
                            path,
                            query,
                            headers: headers.clone(),
                            body: body_bytes.to_vec(),
                        };
                        seen_c3.lock().unwrap().push(s.clone());
                        let (status, rheaders, rbody) = resp_fn3(&s);
                        let mut resp = Response::new(Full::new(rbody));
                        *resp.status_mut() = status;
                        for (n, v) in rheaders {
                            if let Ok(name) = hyper::header::HeaderName::from_bytes(n.as_bytes()) {
                                if let Ok(val) = hyper::header::HeaderValue::from_str(&v) {
                                    resp.headers_mut().insert(name, val);
                                }
                            }
                        }
                        Ok::<_, std::convert::Infallible>(resp)
                    }
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    (sock, seen)
}

#[tokio::test]
async fn forward_socket_round_trip_binary_and_headers() {
    let (sock, seen) = start_simple_socket_server(|_| {
        (
            hyper::StatusCode::OK,
            vec![("x-custom".to_string(), "ok".to_string())],
            Bytes::from(vec![0u8, 1, 2, 255]),
        )
    })
    .await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let keys = HashMap::from([("k".to_string(), Secret::new("key123"))]);
    let app = App::new(config, keys, Policy::default(), PluginChain::default());
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/zen/chat?x=1"))
        .bearer_auth("tok")
        .body(vec![9u8, 10, 11])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap(),
        "4"
    );
    assert_eq!(
        resp.headers().get("x-custom").unwrap().to_str().unwrap(),
        "ok"
    );
    let got = resp.bytes().await.unwrap().to_vec();
    assert_eq!(got, vec![0u8, 1, 2, 255]);
    let seen_list = seen.lock().unwrap();
    assert_eq!(seen_list.len(), 1);
    let s = &seen_list[0];
    assert_eq!(s.method, "POST");
    assert_eq!(s.path, "/v1/chat");
    assert_eq!(s.query.as_deref(), Some("x=1"));
    assert!(s.headers.iter().any(
        |(n, v)| n.eq_ignore_ascii_case("host") && String::from_utf8_lossy(v) == "example.test"
    ));
    assert!(s
        .headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("x-forwarded-proto")
            && String::from_utf8_lossy(v) == "https"));
    assert!(s.headers.iter().any(
        |(n, v)| n.eq_ignore_ascii_case("connection") && String::from_utf8_lossy(v) == "close"
    ));
    assert!(s
        .headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("authorization")
            && String::from_utf8_lossy(v) == "Bearer key123"));
    assert!(!s
        .headers
        .iter()
        .any(|(n, _)| n.to_ascii_lowercase().starts_with("x-reiki")));
    assert_eq!(s.body, vec![9u8, 10, 11]);
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_passthrough() {
    let (sock, seen) =
        start_simple_socket_server(|_| (hyper::StatusCode::OK, vec![], Bytes::from("ok"))).await;
    let text = format!(
        "[upstream.direct]\nbase_url = \"https://example.test/v1\"\npassthrough = true\n\
         [upstream.direct.forward]\nsocket = \"{}\"\n",
        sock.display()
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::new(),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/direct/chat"))
        .bearer_auth("client-own")
        .body("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let seen_list = seen.lock().unwrap();
    assert_eq!(seen_list.len(), 1);
    let s = &seen_list[0];
    assert!(s
        .headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("authorization")
            && String::from_utf8_lossy(v) == "Bearer client-own"));
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_large_bodies_timeout_10s() {
    let (sock, _seen) = start_simple_socket_server(|_| {
        (
            hyper::StatusCode::OK,
            vec![("content-length".to_string(), (1024 * 1024).to_string())],
            Bytes::from(vec![b'x'; 1024 * 1024]),
        )
    })
    .await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let resp = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body(vec![b'y'; 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.bytes().await.unwrap().len(), 1024 * 1024);
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_failover_between_keys() {
    let (sock, seen) = start_simple_socket_server(|s| {
        let auth = s
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("authorization"))
            .and_then(|(_, v)| String::from_utf8(v.clone()).ok())
            .unwrap_or_default();
        if auth == "Bearer key-one" {
            (hyper::StatusCode::UNAUTHORIZED, vec![], Bytes::from("no"))
        } else if auth == "Bearer key-two" {
            (hyper::StatusCode::OK, vec![], Bytes::from("ok"))
        } else {
            (
                hyper::StatusCode::INTERNAL_SERVER_ERROR,
                vec![],
                Bytes::from("bad"),
            )
        }
    })
    .await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k1]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [keys.k2]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k1\", \"k2\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([
            ("k1".to_string(), Secret::new("key-one")),
            ("k2".to_string(), Secret::new("key-two")),
        ]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let seen_list = seen.lock().unwrap();
    assert_eq!(seen_list.len(), 2);
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_missing_path_502_no_cooldown() {
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let resp1 = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 502);
    let resp2 = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 502);
}

#[tokio::test]
async fn forward_socket_close_without_response_502_no_retry() {
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let conn_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = Arc::clone(&conn_count);
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                cc.fetch_add(1, Ordering::SeqCst);
                let _ = AsyncWriteExt::shutdown(&mut stream).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let resp1 = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 502);
    let resp2 = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 502);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(conn_count.load(Ordering::SeqCst), 2);
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_various_bad_responses() {
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = AsyncWriteExt::write_all(&mut stream, b"not http at all\r\n\r\n").await;
                let _ = AsyncWriteExt::shutdown(&mut stream).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(format!("{base}/zen/chat"))
            .bearer_auth("tok")
            .body("{}")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        502
    );
    let _ = std::fs::remove_file(&sock);

    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let body = vec![b'x'; 8 * 1024 * 1024 + 100];
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = AsyncWriteExt::write_all(&mut stream, hdr.as_bytes()).await;
                let _ = AsyncWriteExt::write_all(&mut stream, &body).await;
                let _ = AsyncWriteExt::shutdown(&mut stream).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    assert_eq!(
        client
            .post(format!("{base}/zen/chat"))
            .bearer_auth("tok")
            .body("{}")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        502
    );
    let _ = std::fs::remove_file(&sock);

    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut hdr =
                    String::from("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n");
                for i in 0..129 {
                    hdr.push_str(&format!("x-h{}: v\r\n", i));
                }
                hdr.push_str("\r\n");
                let _ = AsyncWriteExt::write_all(&mut stream, hdr.as_bytes()).await;
                let _ = AsyncWriteExt::write_all(&mut stream, b"ok").await;
                let _ = AsyncWriteExt::shutdown(&mut stream).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    assert_eq!(
        client
            .post(format!("{base}/zen/chat"))
            .bearer_auth("tok")
            .body("{}")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        502
    );
    let _ = std::fs::remove_file(&sock);

    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let bigv = "a".repeat(65537);
                let hdr = format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nx-big: {}\r\nConnection: close\r\n\r\n", bigv);
                let _ = AsyncWriteExt::write_all(&mut stream, hdr.as_bytes()).await;
                let _ = AsyncWriteExt::write_all(&mut stream, b"ok").await;
                let _ = AsyncWriteExt::shutdown(&mut stream).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    assert_eq!(
        client
            .post(format!("{base}/zen/chat"))
            .bearer_auth("tok")
            .body("{}")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        502
    );
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_hung_header_timeout() {
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((_stream, _)) = listener.accept().await {
                // never answer
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::with_header_timeout(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        Duration::from_millis(300),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let start = Instant::now();
    let resp = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    assert!(start.elapsed() < Duration::from_secs(5));
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_expect_uid() {
    use std::os::unix::fs::MetadataExt;
    let myuid = std::fs::metadata("/proc/self").unwrap().uid();
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let req_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bytes_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rc = Arc::clone(&req_count);
    let bc = Arc::clone(&bytes_count);
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                rc.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let mut total = 0;
                let _ = tokio::time::timeout(Duration::from_millis(200), async {
                    loop {
                        match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                total += n;
                            }
                            Err(_) => break,
                        }
                    }
                })
                .await;
                bc.store(total, Ordering::SeqCst);
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\nexpect_uid = {}\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        myuid + 1,
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        req_count.load(Ordering::SeqCst),
        1,
        "connected for the uid check"
    );
    assert_eq!(
        bytes_count.load(Ordering::SeqCst),
        0,
        "nothing may be written"
    );
    let _ = std::fs::remove_file(&sock);

    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = tokio::time::timeout(Duration::from_millis(500), async {
                    let mut buf = vec![0u8; 4096];
                    let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                })
                .await;
                let _ = AsyncWriteExt::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await;
                let _ = AsyncWriteExt::shutdown(&mut stream).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\nexpect_uid = {}\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        myuid,
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_ack_202() {
    let (sock, _seen) = start_simple_socket_server(|_| {
        (
            hyper::StatusCode::ACCEPTED,
            vec![("content-type".to_string(), "application/json".to_string())],
            Bytes::from("{\"ok\":true}"),
        )
    })
    .await;
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    assert_eq!(resp.text().await.unwrap(), "{\"ok\":true}");
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_chunked_response() {
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = tokio::time::timeout(Duration::from_millis(200), async {
                    while let Ok(n) = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                        if n == 0 {
                            break;
                        }
                    }
                })
                .await;
                let _ = AsyncWriteExt::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await;
                let _ = AsyncWriteExt::write_all(
                    &mut stream,
                    b"2\r\nhe\r\n3\r\nllo\r\n1\r\n!\r\n0\r\n\r\n",
                )
                .await;
                let _ = AsyncWriteExt::shutdown(&mut stream).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.headers().get("content-length").is_some());
    assert_eq!(resp.text().await.unwrap(), "hello!");
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_socket_early_reply_no_hang() {
    let sock = unique_sock_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        #[allow(clippy::while_let_loop)]
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = AsyncWriteExt::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            } else {
                break;
            }
        }
    });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut ok = 0;
    let mut bad = 0;
    for _ in 0..20 {
        if let Ok(resp) = client
            .post(format!("{base}/zen/chat"))
            .bearer_auth("tok")
            .body(vec![b'a'; 1024 * 1024])
            .send()
            .await
        {
            let s = resp.status().as_u16();
            if s == 200 {
                ok += 1;
            } else if s == 502 {
                bad += 1;
            }
        }
    }
    assert_eq!(
        ok, 20,
        "an early reply must still reach the client ({bad} failed)"
    );
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn forward_command_echo_round_trip() {
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"{}\"\nargs = [\"{}\"]\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        python(),
        child_script().display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat?x=1"))
        .bearer_auth("tok")
        .body(vec![1, 2, 3, 4])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("x-saw-method")
            .unwrap()
            .to_str()
            .unwrap(),
        "POST"
    );
    assert_eq!(
        resp.headers()
            .get("x-saw-target")
            .unwrap()
            .to_str()
            .unwrap(),
        "/v1/chat?x=1"
    );
    assert_eq!(
        resp.headers().get("x-saw-host").unwrap().to_str().unwrap(),
        "example.test"
    );
    assert_eq!(
        resp.headers()
            .get("x-saw-authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer key123"
    );
    assert_eq!(
        resp.headers().get("x-saw-xfp").unwrap().to_str().unwrap(),
        "https"
    );
    assert_eq!(resp.bytes().await.unwrap().to_vec(), vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn forward_command_passthrough() {
    let text = format!(
        "[upstream.direct]\nbase_url = \"https://example.test/v1\"\npassthrough = true\n\
         [upstream.direct.forward]\ncommand = \"{}\"\nargs = [\"{}\"]\n",
        python(),
        child_script().display()
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::new(),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/direct/chat"))
        .bearer_auth("client-own")
        .body("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("x-saw-authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer client-own"
    );
}

#[tokio::test]
async fn forward_command_big_both_ways() {
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"{}\"\nargs = [\"{}\", \"big\"]\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        python(),
        child_script().display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body(vec![b'a'; 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.bytes().await.unwrap().len(), 1024 * 1024);
}

#[tokio::test]
async fn forward_command_exit_silent() {
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"{}\"\nargs = [\"{}\", \"exit-silent\"]\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        python(),
        child_script().display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let client = reqwest::Client::new();
    let resp1 = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 502);
    let resp2 = client
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 502);
}

#[tokio::test]
async fn forward_command_missing_binary() {
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"/no/such/bin\"\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
}

fn pid_alive(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&out.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

async fn wait_until_dead(pid: u32, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "child {pid} still running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn forward_command_hang_header_timeout() {
    let pid_path = std::env::temp_dir().join(format!("reiki-fwd-hang-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&pid_path);
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"{}\"\nargs = [\"{}\", \"hang\", \"{}\"]\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        python(),
        child_script().display(),
        pid_path.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::with_header_timeout(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        Duration::from_millis(300),
    );
    let base = serve_app(app).await;
    let start = Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    assert!(start.elapsed() < Duration::from_secs(5));
    let pid: u32 = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_until_dead(pid, 2).await;
    let _ = std::fs::remove_file(&pid_path);
}

#[tokio::test]
async fn forward_command_linger() {
    let pid_path =
        std::env::temp_dir().join(format!("reiki-fwd-linger-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&pid_path);
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"{}\"\nargs = [\"{}\", \"linger\", \"{}\"]\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        python(),
        child_script().display(),
        pid_path.display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let start = Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(start.elapsed() < Duration::from_secs(5));
    let mut pid = None;
    for _ in 0..50 {
        if pid_path.exists() {
            if let Ok(s) = std::fs::read_to_string(&pid_path) {
                if let Ok(p) = s.trim().parse::<u32>() {
                    pid = Some(p);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = pid.unwrap();
    wait_until_dead(pid, 3).await;
    let _ = std::fs::remove_file(&pid_path);
}

#[tokio::test]
async fn forward_command_env_report() {
    std::env::set_var("REIKI_TEST_SECRET", "x");
    std::env::set_var("REIKI_TEST_PASSED", "y");
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"{}\"\nargs = [\"{}\", \"env-report\", \"REIKI_TEST_SECRET\", \"REIKI_TEST_PASSED\", \"PATH\"]\npass_env = [\"REIKI_TEST_PASSED\"]\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        python(),
        child_script().display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        config,
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let report = resp.text().await.unwrap();
    assert!(report.contains("REIKI_TEST_SECRET=0"), "{report}");
    assert!(report.contains("REIKI_TEST_PASSED=1"), "{report}");
    assert!(report.contains("PATH=1"), "{report}");
    assert!(report.contains("token_in_env=0"), "{report}");
}

async fn relay_for(sock: &std::path::Path, extra: &str) -> String {
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n{extra}\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let app = App::new(
        Config::parse(&text).unwrap(),
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    serve_app(app).await
}

#[tokio::test]
async fn head_keeps_the_length_the_forwarder_reported() {
    let (sock, _seen) = start_simple_socket_server(|_| {
        (
            hyper::StatusCode::OK,
            vec![("content-length".to_string(), "5".to_string())],
            Bytes::from("hello"),
        )
    })
    .await;
    let base = relay_for(&sock, "").await;
    let resp = reqwest::Client::new()
        .head(format!("{base}/zen/models"))
        .bearer_auth("tok")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.headers().get("content-length").unwrap(), "5");
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn no_content_carries_no_content_length() {
    let (sock, _seen) =
        start_simple_socket_server(|_| (hyper::StatusCode::NO_CONTENT, vec![], Bytes::new())).await;
    let base = relay_for(&sock, "").await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert!(resp.headers().get("content-length").is_none());
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn expect_continue_is_not_passed_to_the_forwarder() {
    let (sock, seen) =
        start_simple_socket_server(|_| (hyper::StatusCode::OK, vec![], Bytes::from("ok"))).await;
    let base = relay_for(&sock, "").await;
    let resp = tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::Client::new()
            .post(format!("{base}/zen/chat"))
            .bearer_auth("tok")
            .header("Expect", "100-continue")
            .body("{}")
            .send(),
    )
    .await
    .expect("stalled")
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let seen = seen.lock().unwrap();
    assert!(!seen[0]
        .headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("expect")));
    let _ = std::fs::remove_file(&sock);
}

async fn relay_with_fallback(sock: &std::path::Path) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let other = axum::Router::new().fallback(move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            "from-fallback"
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, other).await.unwrap() });
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\nsocket = \"{}\"\n\
         [upstream.zen.fallback]\nupstream = \"other\"\n\
         [upstream.other]\nbase_url = \"http://{addr}/api\"\n\
         [keys.z]\nupstream = \"zen\"\nfile = \"/unused/z\"\n\
         [keys.o]\nupstream = \"other\"\nfile = \"/unused/o\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"z\", \"o\"]\n",
        sock.display(),
        reiki::auth::sha256_hex("tok")
    );
    let app = App::new(
        Config::parse(&text).unwrap(),
        HashMap::from([
            ("z".to_string(), Secret::new("key-z")),
            ("o".to_string(), Secret::new("key-o")),
        ]),
        Policy::default(),
        PluginChain::default(),
    );
    (serve_app(app).await, hits)
}

#[tokio::test]
async fn a_link_failure_does_not_trigger_the_fallback_upstream() {
    let sock = unique_sock_path();
    let (base, hits) = relay_with_fallback(&sock).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_forwarder_that_exhausts_its_keys_still_falls_back() {
    let (sock, _seen) = start_simple_socket_server(|_| {
        (
            hyper::StatusCode::PAYMENT_REQUIRED,
            vec![],
            Bytes::from("no funds"),
        )
    })
    .await;
    let (base, hits) = relay_with_fallback(&sock).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/zen/chat"))
        .bearer_auth("tok")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.text().await.unwrap(), "from-fallback");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn too_many_copies_of_a_command_are_refused() {
    let pid_path = std::env::temp_dir().join(format!("reiki-fwd-cap-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&pid_path);
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.test/v1\"\n\
         [upstream.zen.forward]\ncommand = \"{}\"\nargs = [\"{}\", \"hang\", \"{}\"]\nmax_concurrent = 1\n\
         [keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        python(),
        child_script().display(),
        pid_path.display(),
        reiki::auth::sha256_hex("tok")
    );
    let app = App::new(
        Config::parse(&text).unwrap(),
        HashMap::from([("k".to_string(), Secret::new("key123"))]),
        Policy::default(),
        PluginChain::default(),
    );
    let base = serve_app(app).await;
    let url = format!("{base}/zen/chat");
    let first = tokio::spawn({
        let url = url.clone();
        async move {
            let _ = reqwest::Client::new()
                .post(url)
                .bearer_auth("tok")
                .body("{}")
                .send()
                .await;
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let second = tokio::time::timeout(
        Duration::from_secs(3),
        reqwest::Client::new()
            .post(&url)
            .bearer_auth("tok")
            .body("{}")
            .send(),
    )
    .await
    .expect("the second request waited instead of being refused")
    .unwrap();
    assert_eq!(second.status().as_u16(), 502);
    first.abort();
    let _ = std::fs::remove_file(&pid_path);
}
