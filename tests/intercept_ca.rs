//! A reload swaps the interception CA: new connections verify against the new CA, open connections keep their leaf, and the watcher picks up a renewed CA file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::intercept::{init_ca, Interceptor};
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, App};
use reiki::tunnel::{serve, Tunnel};
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::client::TlsStream;

const HOST: &str = "api.test.example";
const TOKEN: &str = "tok";

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reiki-ca-swap-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn upstream() -> u16 {
    let app = Router::new().fallback(|| async { "ok" });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

struct Rig {
    config: PathBuf,
    ca_dir: PathBuf,
    app: Arc<App>,
    tunnel: String,
}

impl Rig {
    async fn new(label: &str, reload: &str) -> Self {
        let dir = scratch(label);
        let ca_dir = dir.join("ca");
        init_ca(&ca_dir, &[HOST.to_owned()], 30, false).unwrap();
        std::fs::write(dir.join("key"), "upstream-secret\n").unwrap();
        let port = upstream().await;
        let config = dir.join("reiki.toml");
        let text = format!(
            r#"
            {reload}
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
            hosts = [{{ host = "{HOST}", upstream = "api", stream = "s" }}]
            "#,
            key = dir.join("key").display(),
            hash = reiki::auth::sha256_hex(TOKEN),
            ca = ca_dir.display(),
        );
        std::fs::write(&config, &text).unwrap();
        let config_text = Config::parse(&text).unwrap();
        let keys = load_keys(&config_text).unwrap();
        let tunnel_config = Config::parse(&text).unwrap().tunnel.unwrap();
        let app = App::new(config_text, keys, Policy::default(), PluginChain::default());
        app.set_config_path(config.clone());
        let interceptor =
            Interceptor::new(Arc::clone(&app), tunnel_config.intercept.as_ref().unwrap()).unwrap();
        let tunnel = Arc::new(Tunnel::new(&tunnel_config).with_interceptor(Arc::new(interceptor)));
        app.set_tunnel(Arc::clone(&tunnel));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(serve(listener, tunnel));
        Self {
            config,
            ca_dir,
            app,
            tunnel: addr,
        }
    }

    /// Opens an intercepted CONNECT and runs the client handshake, trusting only `trust`.
    async fn tls(&self, trust: &[u8]) -> std::io::Result<TlsStream<TcpStream>> {
        let mut stream = TcpStream::connect(&self.tunnel).await?;
        stream
            .write_all(
                format!(
                    "CONNECT {HOST}:443 HTTP/1.1\r\nHost: {HOST}\r\nProxy-Authorization: Bearer {TOKEN}\r\n\r\n"
                )
                .as_bytes(),
            )
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
}

/// Sends one keep-alive GET and returns the status of the answer.
async fn get(tls: &mut TlsStream<TcpStream>) -> u16 {
    tls.write_all(format!("GET /v1/x HTTP/1.1\r\nHost: {HOST}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        tls.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .expect("the answer has a length")
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0u8; length];
    tls.read_exact(&mut body).await.unwrap();
    status
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[tokio::test]
async fn a_reload_swaps_the_ca_for_new_connections() {
    let rig = Rig::new("swap", "").await;
    let old_pem = read(&rig.ca_dir.join("ca.pem"));
    let mut in_flight = rig.tls(&old_pem).await.unwrap();
    assert_eq!(get(&mut in_flight).await, 200);

    init_ca(&rig.ca_dir, &[HOST.to_owned()], 30, true).unwrap();
    let new_pem = read(&rig.ca_dir.join("ca.pem"));
    assert_ne!(old_pem, new_pem);
    let report = rig.app.reload_from_file(&rig.config).unwrap();
    assert!(
        report.changed.contains(&"intercept ca".to_owned()),
        "{:?}",
        report.changed
    );

    assert!(
        rig.tls(&old_pem).await.is_err(),
        "a new connection does not verify against the old CA"
    );
    let mut fresh = rig.tls(&new_pem).await.unwrap();
    assert_eq!(get(&mut fresh).await, 200);

    assert_eq!(
        get(&mut in_flight).await,
        200,
        "the connection opened before the swap keeps working"
    );
}

#[tokio::test]
async fn a_refused_ca_leaves_the_running_one_serving() {
    let rig = Rig::new("refuse", "").await;
    let old_pem = read(&rig.ca_dir.join("ca.pem"));
    init_ca(
        &rig.ca_dir,
        &["elsewhere.test.example".to_owned()],
        30,
        true,
    )
    .unwrap();

    let error = rig
        .app
        .reload_from_file(&rig.config)
        .expect_err("a CA that does not cover the host is refused");
    assert!(error.contains(HOST), "{error}");
    let mut tls = rig.tls(&old_pem).await.unwrap();
    assert_eq!(get(&mut tls).await, 200);
}

#[tokio::test]
async fn the_watcher_picks_up_a_renewed_ca_file() {
    let rig = Rig::new("watch", "[reload]\nwatch = true\ninterval_secs = 1\n").await;
    assert!(rig.app.reload_inputs().contains(&rig.ca_dir.join("ca.pem")));
    tokio::spawn(reiki::reload::watch(Arc::clone(&rig.app)));
    tokio::time::sleep(Duration::from_millis(1500)).await;

    init_ca(&rig.ca_dir, &[HOST.to_owned()], 30, true).unwrap();
    let new_pem = read(&rig.ca_dir.join("ca.pem"));
    let mut trusted = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if rig.tls(&new_pem).await.is_ok() {
            trusted = true;
            break;
        }
    }
    assert!(trusted, "the watcher applied the renewed CA");
    assert!(rig.app.reload_status_json().contains("\"generation\":1"));
}
