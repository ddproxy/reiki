use std::path::PathBuf;
use std::sync::Arc;

use reiki::chain::Policy;
use reiki::config::Config;
use reiki::intercept::{init_ca, Interceptor};
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, router, App};
use reiki::tunnel::{serve, Tunnel};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const HOST: &str = "api.test.example";
const REAL_KEY: &str = "real-upstream-key";

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reiki-faithful-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Accepts one request and returns its head and body exactly as they arrived.
async fn upstream() -> (u16, JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head);
        let length = text
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        let mut raw = head;
        raw.extend_from_slice(&body);
        raw
    });
    (port, task)
}

struct Setup {
    tunnel: String,
    ca_pem: Vec<u8>,
    upstream_port: u16,
}

async fn setup(label: &str) -> (Setup, JoinHandle<Vec<u8>>) {
    let dir = scratch(label);
    init_ca(&dir.join("ca"), &[HOST.to_owned()], 30, false).unwrap();
    std::fs::write(dir.join("key"), REAL_KEY).unwrap();
    let (upstream_port, captured) = upstream().await;
    let text = format!(
        r#"
        [upstream.api]
        base_url = "http://127.0.0.1:{upstream_port}/v1"
        [keys.k1]
        upstream = "api"
        file = "{key}"
        [streams.s]
        token_sha256 = "{hash}"
        chain = ["k1"]
        [tunnel]
        listen = "127.0.0.1:1"
        allow = ["{HOST}"]
        [tunnel.intercept]
        ca_cert = "{ca}/ca.pem"
        ca_key = "{ca}/ca.key"
        hosts = [{{ host = "{HOST}", upstream = "api", stream = "s" }}]
        "#,
        key = dir.join("key").display(),
        hash = reiki::auth::sha256_hex("unused"),
        ca = dir.join("ca").display(),
    );
    let mut config = Config::parse(&text).unwrap();
    let keys = load_keys(&config).unwrap();
    let tunnel_config = config.tunnel.take().unwrap();
    let app = App::new(config, keys, Policy::default(), PluginChain::default());
    let interceptor =
        Interceptor::new(Arc::clone(&app), tunnel_config.intercept.as_ref().unwrap()).unwrap();
    let tunnel = Tunnel::new(&tunnel_config).with_interceptor(Arc::new(interceptor));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(serve(listener, Arc::new(tunnel)));
    let setup = Setup {
        tunnel: addr,
        ca_pem: std::fs::read(dir.join("ca/ca.pem")).unwrap(),
        upstream_port,
    };
    (setup, captured)
}

async fn connect(
    setup: &Setup,
    trust: &[u8],
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut stream = TcpStream::connect(&setup.tunnel).await?;
    stream
        .write_all(format!("CONNECT {HOST}:443 HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await?;
        head.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));

    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(trust) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(HOST).unwrap(), stream)
        .await
}

async fn read_response<S>(stream: &mut S) -> (u16, Vec<u8>)
where
    S: AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).await.unwrap();
        if read == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let length = text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await.unwrap();
    (status, body)
}

async fn send(setup: &Setup, head: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut tls = connect(setup, &setup.ca_pem).await.unwrap();
    tls.write_all(head.as_bytes()).await.unwrap();
    tls.write_all(body).await.unwrap();
    tls.flush().await.unwrap();
    read_response(&mut tls).await
}

fn header_lines(raw: &str) -> Vec<&str> {
    raw.split("\r\n\r\n")
        .next()
        .unwrap_or(raw)
        .lines()
        .skip(1)
        .filter(|line| !line.is_empty())
        .collect()
}

#[tokio::test]
async fn an_intercepted_request_reaches_the_upstream_as_the_client_sent_it() {
    let (setup, captured) = setup("as-sent").await;
    let body = br#"{"model":"gpt-5"}"#;
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: {HOST}\r\n\
         Authorization: Bearer client-key\r\n\
         Content-Type: application/json\r\n\
         User-Agent: opencode/1.0\r\n\
         x-lower-one: one\r\n\
         x-lower-two: two\r\n\
         Connection: keep-alive\r\n\
         Accept: */*\r\n\
         Accept-Encoding: gzip, deflate, br, zstd\r\n\
         Content-Length: {}\r\n\
         \r\n",
        body.len()
    );
    let (status, response) = send(&setup, &head, body).await;
    assert_eq!(status, 200);
    assert_eq!(response, b"ok");

    let raw = String::from_utf8_lossy(&captured.await.unwrap()).into_owned();
    let lines = header_lines(&raw);
    let authority = format!("127.0.0.1:{}", setup.upstream_port);
    assert_eq!(lines[0], format!("Host: {authority}"), "{raw}");
    assert_eq!(
        lines[1],
        format!("Authorization: Bearer {REAL_KEY}"),
        "{raw}"
    );
    assert_eq!(lines[2], "Content-Type: application/json", "{raw}");
    assert_eq!(lines[3], "User-Agent: opencode/1.0", "{raw}");
    assert_eq!(lines[4], "x-lower-one: one", "{raw}");
    assert_eq!(lines[5], "x-lower-two: two", "{raw}");
    assert_eq!(lines[6], "Connection: keep-alive", "{raw}");
    assert_eq!(lines[7], "Accept: */*", "{raw}");
    assert_eq!(
        lines[8], "Accept-Encoding: gzip, deflate, br, zstd",
        "{raw}"
    );
    assert_eq!(lines[9], format!("Content-Length: {}", body.len()), "{raw}");
    assert_eq!(lines.len(), 10, "{raw}");

    assert!(!raw.to_ascii_lowercase().contains("identity"), "{raw}");
    assert!(!raw.contains("client-key"), "{raw}");
    assert!(raw.ends_with(std::str::from_utf8(body).unwrap()), "{raw}");
}

#[tokio::test]
async fn a_client_connection_close_is_not_forwarded() {
    let (setup, captured) = setup("close").await;
    let body = b"{}";
    let head = format!(
        "POST /v1/x HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer client-key\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let (status, _) = send(&setup, &head, body).await;
    assert_eq!(status, 200);

    let raw = String::from_utf8_lossy(&captured.await.unwrap()).into_owned();
    assert!(
        !raw.to_ascii_lowercase().contains("\r\nconnection:"),
        "{raw}"
    );
}

#[tokio::test]
async fn only_the_credential_is_added_when_the_client_sent_none() {
    let (setup, captured) = setup("minimal").await;
    let body = b"{}";
    let head = format!(
        "POST /v1/x HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let (status, _) = send(&setup, &head, body).await;
    assert_eq!(status, 200);

    let raw = String::from_utf8_lossy(&captured.await.unwrap()).into_owned();
    let lines = header_lines(&raw);
    let names: Vec<String> = lines
        .iter()
        .map(|line| line.split(':').next().unwrap().to_ascii_lowercase())
        .collect();
    assert_eq!(names, ["host", "authorization", "content-length"], "{raw}");
    assert_eq!(
        lines[1].to_ascii_lowercase(),
        format!("authorization: bearer {}", REAL_KEY.to_ascii_lowercase())
    );
    assert!(raw.ends_with("{}"), "{raw}");
}

#[tokio::test]
async fn the_reverse_proxy_route_still_lowercases_and_sends_identity() {
    let (port, captured) = upstream().await;
    let dir = scratch("reverse");
    std::fs::write(dir.join("key"), REAL_KEY).unwrap();
    let text = format!(
        "[upstream.api]\nbase_url = \"http://127.0.0.1:{port}/v1\"\n\
         [keys.k]\nupstream = \"api\"\nfile = \"{}\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        dir.join("key").display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let keys = load_keys(&config).unwrap();
    let app = App::new(config, keys, Policy::default(), PluginChain::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/api/chat"))
        .bearer_auth("tok")
        .header("X-Custom-Thing", "1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let raw = String::from_utf8_lossy(&captured.await.unwrap()).into_owned();
    let lines = header_lines(&raw);
    for line in &lines {
        let name = line.split(':').next().unwrap();
        assert_eq!(name, name.to_ascii_lowercase(), "{raw}");
    }
    assert!(lines.contains(&"accept-encoding: identity"), "{raw}");
}
