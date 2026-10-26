use std::path::PathBuf;
use std::sync::Arc;

use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZlibEncoder, ZstdEncoder};
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::intercept::{init_ca, Interceptor};
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, App};
use reiki::tunnel::{serve, Tunnel};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const HOST: &str = "api.test.example";
const REAL_KEY: &str = "real-upstream-key";

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reiki-decode-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
    })
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        line.split_once(':')
            .filter(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    })
}

/// Accepts one request, then answers it with `response` verbatim.
async fn upstream(response: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let length = content_length(&String::from_utf8_lossy(&head)).unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.unwrap();
        stream.write_all(&response).await.unwrap();
        stream.flush().await.unwrap();
    });
    port
}

fn encoded_response(status: &str, encoding: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

async fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzipEncoder::new(Vec::new());
    encoder.write_all(data).await.unwrap();
    encoder.shutdown().await.unwrap();
    encoder.into_inner()
}

async fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new());
    encoder.write_all(data).await.unwrap();
    encoder.shutdown().await.unwrap();
    encoder.into_inner()
}

async fn brotli(data: &[u8]) -> Vec<u8> {
    let mut encoder = BrotliEncoder::new(Vec::new());
    encoder.write_all(data).await.unwrap();
    encoder.shutdown().await.unwrap();
    encoder.into_inner()
}

async fn zstd(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZstdEncoder::new(Vec::new());
    encoder.write_all(data).await.unwrap();
    encoder.shutdown().await.unwrap();
    encoder.into_inner()
}

struct Setup {
    tunnel: String,
    ca_pem: Vec<u8>,
}

async fn setup(label: &str, response: Vec<u8>) -> Setup {
    let dir = scratch(label);
    init_ca(&dir.join("ca"), &[HOST.to_owned()], 30, false).unwrap();
    std::fs::write(dir.join("key"), REAL_KEY).unwrap();
    let upstream_port = upstream(response).await;
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
    Setup {
        tunnel: addr,
        ca_pem: std::fs::read(dir.join("ca/ca.pem")).unwrap(),
    }
}

async fn connect(setup: &Setup) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
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
    for cert in CertificateDer::pem_slice_iter(&setup.ca_pem) {
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

async fn read_line<S>(stream: &mut S) -> String
where
    S: AsyncRead + Unpin,
{
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).await.unwrap();
        if read == 0 || byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8_lossy(&line)
        .trim_end_matches('\r')
        .to_owned()
}

async fn read_response<S>(stream: &mut S) -> (u16, String, Vec<u8>)
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
    let head = String::from_utf8_lossy(&head).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    let mut body = Vec::new();
    if let Some(length) = content_length(&head) {
        body.resize(length, 0);
        stream.read_exact(&mut body).await.unwrap();
    } else if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        loop {
            let line = read_line(stream).await;
            let size = usize::from_str_radix(line.split(';').next().unwrap().trim(), 16).unwrap();
            if size == 0 {
                while !read_line(stream).await.is_empty() {}
                break;
            }
            let mut chunk = vec![0u8; size];
            stream.read_exact(&mut chunk).await.unwrap();
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            stream.read_exact(&mut crlf).await.unwrap();
        }
    }
    (status, head, body)
}

async fn send(setup: &Setup, body: &[u8]) -> (u16, String, Vec<u8>) {
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: {HOST}\r\n\
         Authorization: Bearer client-key\r\n\
         Content-Type: application/json\r\n\
         Accept-Encoding: gzip, deflate, br, zstd\r\n\
         Content-Length: {}\r\n\
         \r\n",
        body.len()
    );
    let mut tls = connect(setup).await.unwrap();
    tls.write_all(head.as_bytes()).await.unwrap();
    tls.write_all(body).await.unwrap();
    tls.flush().await.unwrap();
    read_response(&mut tls).await
}

#[tokio::test]
async fn encoded_bodies_arrive_as_plain_bytes_without_encoding_headers() {
    let payload = b"the upstream said something useful";
    let cases = [
        ("gzip", gzip(payload).await),
        ("deflate", zlib(payload).await),
        ("br", brotli(payload).await),
        ("zstd", zstd(payload).await),
    ];
    for (encoding, encoded) in cases {
        let label = format!("plain-{encoding}");
        let setup = setup(&label, encoded_response("200 OK", encoding, &encoded)).await;
        let (status, head, body) = send(&setup, payload).await;
        assert_eq!(status, 200, "{encoding}: {head}");
        assert_eq!(body, payload, "{encoding}: {head}");
        assert!(
            header(&head, "content-encoding").is_none(),
            "{encoding}: {head}"
        );
        assert!(
            header(&head, "content-length").is_none(),
            "{encoding}: {head}"
        );
    }
}

#[tokio::test]
async fn a_compressed_error_body_reaches_the_caller_as_text() {
    let payload = br#"{"error":{"message":"bad request"}}"#;
    let encoded = gzip(payload).await;
    let setup = setup(
        "error",
        encoded_response("400 Bad Request", "gzip", &encoded),
    )
    .await;
    let (status, head, body) = send(&setup, b"{}").await;
    assert_eq!(status, 400, "{head}");
    assert_eq!(body, payload, "{head}");
    assert!(header(&head, "content-encoding").is_none(), "{head}");
}

#[tokio::test]
async fn an_unknown_encoding_passes_through_untouched() {
    let payload = b"not really lzma";
    let setup = setup("unknown", encoded_response("200 OK", "lzma", payload)).await;
    let (status, head, body) = send(&setup, b"{}").await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, payload, "{head}");
    assert_eq!(header(&head, "content-encoding"), Some("lzma"), "{head}");
    assert_eq!(
        header(&head, "content-length").map(|value| value.parse().unwrap()),
        Some(payload.len()),
        "{head}"
    );
}
