use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::intercept::{init_ca, Interceptor};
use reiki::plugins::{PluginChain, Registration};
use reiki::proxy::{load_keys, App};
use reiki::reiki_plugin::{Plugin, RequestCtx};
use reiki::tunnel::{serve, Tunnel};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const HOST: &str = "api.test.example";
const REAL_KEY: &str = "real-upstream-key";

struct Tag;

impl Plugin for Tag {
    fn name(&self) -> &str {
        "tag"
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Result<(), reiki::reiki_plugin::PluginError> {
        ctx.headers
            .insert("x-tag", axum::http::HeaderValue::from_static("seen"));
        Ok(())
    }
}

fn tag_registry() -> Vec<Registration> {
    vec![Registration {
        name: "tag",
        build: |_| Ok(Box::new(Tag)),
    }]
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reiki-intercept-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Echoes what the upstream received as `path|authorization|x-tag|body`.
async fn upstream() -> u16 {
    let app = Router::new().fallback(|req: Request| async move {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_owned()
        };
        Response::new(axum::body::Body::from(format!(
            "{}|{}|{}|{}",
            parts.uri.path(),
            header("authorization"),
            header("x-tag"),
            String::from_utf8_lossy(&body)
        )))
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

struct Setup {
    tunnel: String,
    ca_pem: Vec<u8>,
}

async fn setup(label: &str, extra: &str, host_fields: &str) -> Setup {
    let dir = scratch(label);
    init_ca(&dir.join("ca"), &[HOST.to_owned()], 30, false).unwrap();
    std::fs::write(dir.join("key"), REAL_KEY).unwrap();
    let port = upstream().await;
    let text = format!(
        r#"
        [upstream.api]
        base_url = "http://127.0.0.1:{port}/v1"
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
        hosts = [{{ host = "{HOST}", upstream = "api", stream = "s"{host_fields} }}]
        {extra}
        "#,
        key = dir.join("key").display(),
        hash = reiki::auth::sha256_hex("unused"),
        ca = dir.join("ca").display(),
    );
    let mut config = Config::parse(&text).unwrap();
    let keys = load_keys(&config).unwrap();
    let tunnel_config = config.tunnel.take().unwrap();
    let tables: BTreeMap<String, toml::Table> = config.plugins.clone();
    let plugins = PluginChain::build(&tables, &tag_registry()).unwrap();
    let app = App::new(config, keys, Policy::default(), plugins);
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

async fn request(
    setup: &Setup,
    host_header: &str,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, String) {
    let mut tls = connect(setup, &setup.ca_pem).await.unwrap();
    let text = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_header}\r\nAuthorization: Bearer client-supplied\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(text.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw).await;
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
    (status, body)
}

#[tokio::test]
async fn a_request_gets_the_real_key_and_the_plugins() {
    let s = setup("key", "[plugins.tag]", "").await;
    let (status, body) = request(&s, HOST, "POST", "/v1/chat/completions", "{\"a\":1}").await;
    assert_eq!(status, 200, "{body}");
    // With a plugin configured the response is chunked, so only the payload is compared.
    let expected = format!("/v1/chat/completions|Bearer {REAL_KEY}|seen|{{\"a\":1}}");
    assert!(body.contains(&expected), "{body}");
    assert!(!body.contains("client-supplied"));
}

#[tokio::test]
async fn a_path_outside_the_upstream_base_is_refused() {
    let s = setup("path", "", "").await;
    for path in ["/other", "/v10/x", "/"] {
        let (status, _) = request(&s, HOST, "GET", path, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN.as_u16(), "{path}");
    }
}

#[tokio::test]
async fn a_host_header_for_another_name_is_refused() {
    let s = setup("host", "", "").await;
    let (status, _) = request(&s, "elsewhere.test.example", "GET", "/v1/x", "").await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST.as_u16());
}

#[tokio::test]
async fn a_client_that_does_not_trust_the_ca_cannot_connect() {
    let s = setup("trust", "", "").await;
    let other = scratch("other-ca");
    init_ca(&other, &[HOST.to_owned()], 30, false).unwrap();
    let wrong = std::fs::read(other.join("ca.pem")).unwrap();
    assert!(connect(&s, &wrong).await.is_err());
}

#[tokio::test]
async fn the_ca_refuses_names_outside_its_constraint() {
    let dir = scratch("constraint");
    let result = init_ca(&dir, &["*.example.com".to_owned()], 30, false);
    assert!(result.is_err());
    let again = init_ca(&dir.join("ok"), &[HOST.to_owned()], 30, false);
    assert!(again.is_ok());
    assert!(
        init_ca(&dir.join("ok"), &[HOST.to_owned()], 30, false).is_err(),
        "an existing CA is not overwritten"
    );
}

#[tokio::test]
async fn listed_paths_reach_the_host_as_sent() {
    let s = setup("paths", "", r#", paths = ["/inference"]"#).await;
    let (status, body) = request(&s, HOST, "POST", "/inference/openai/v1/chat", "{}").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        format!("/inference/openai/v1/chat|Bearer {REAL_KEY}|-|{{}}")
    );
    for path in ["/v1/chat", "/inference2/x", "/"] {
        let (status, _) = request(&s, HOST, "GET", path, "").await;
        assert_eq!(status, 403, "{path}");
    }
}
