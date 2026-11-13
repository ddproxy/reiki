//! `reiki peers` against mock peers: its own pinned TLS client, only the peer token, no
//! redirects, a body cap, and `unknown` for anything that is not fresh.
#![cfg(feature = "peers")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::peers::client::{poll, PeerReport};
use reiki::proxy::{App, Secret};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PEER_TOKEN: &str = "peer-token-for-cli-tests";

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
}

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("reiki-peers-cli-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, name: &str, text: &str, mode: u32) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A CA and a server configuration whose leaf it signed for `127.0.0.1` and `localhost`.
struct Pki {
    ca_pem: String,
    server: Arc<rustls::ServerConfig>,
}

fn pki() -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    let chain = vec![CertificateDer::from(leaf.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    Pki {
        ca_pem: ca_cert.pem(),
        server: Arc::new(server),
    }
}

/// A peer that answers every request with `reply` and records each request head.
struct MockPeer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
}

async fn mock_peer(tls: Option<Arc<rustls::ServerConfig>>, reply: Vec<u8>) -> MockPeer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (seen, recorded) = (Arc::clone(&hits), Arc::clone(&heads));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (tls, reply) = (tls.clone(), reply.clone());
            let (seen, recorded) = (Arc::clone(&seen), Arc::clone(&recorded));
            tokio::spawn(async move {
                match tls {
                    Some(config) => {
                        let acceptor = tokio_rustls::TlsAcceptor::from(config);
                        if let Ok(stream) = acceptor.accept(stream).await {
                            answer(stream, &reply, &seen, &recorded).await;
                        }
                    }
                    None => answer(stream, &reply, &seen, &recorded).await,
                }
            });
        }
    });
    MockPeer { addr, hits, heads }
}

async fn answer<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: S,
    reply: &[u8],
    hits: &AtomicUsize,
    heads: &Mutex<Vec<String>>,
) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    hits.fetch_add(1, Ordering::SeqCst);
    heads
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&head).into_owned());
    let _ = stream.write_all(reply).await;
    let _ = stream.shutdown().await;
}

fn now_rfc3339(offset_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + offset_secs;
    let days = now.div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rem = now.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn health_json(observed_at: &str) -> String {
    format!(
        "{{\"instance\":\"mock\",\"observed_at\":\"{observed_at}\",\"upstreams\":[\
         {{\"name\":\"zen\",\"healthy_keys\":2,\"cooling_keys\":0,\"disabled_keys\":0,\
         \"soonest_cooldown_seconds\":null,\"last_status_class\":\"2xx\"}}]}}"
    )
}

fn http_reply(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut text = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("content-length: {}\r\n\r\n{body}", body.len()));
    text.into_bytes()
}

fn ok_reply(body: &str) -> Vec<u8> {
    http_reply("200 OK", &[("content-type", "application/json")], body)
}

/// A `[peers]` config whose entries are `(name, url, ca_file)`, all reading `token_file`.
fn peers_config(entries: &[(&str, String, Option<&Path>)], token_file: &Path) -> Config {
    let mut text = String::from("[peers]\nenabled = true\npoll_interval_secs = 30\n");
    for (name, url, ca) in entries {
        text.push_str(&format!(
            "[peers.{name}]\nurl = \"{url}\"\ntoken_file = \"{}\"\n",
            token_file.display()
        ));
        if let Some(ca) = ca {
            text.push_str(&format!("ca_file = \"{}\"\n", ca.display()));
        }
    }
    Config::parse(&text).unwrap()
}

async fn poll_one(entry: (&str, String, Option<&Path>), token_file: &Path) -> PeerReport {
    let config = peers_config(&[entry], token_file);
    poll(config.peers.as_ref().unwrap()).await.remove(0)
}

#[tokio::test]
async fn fresh_data_over_tls_pinned_to_the_peer_ca() {
    let dir = Dir::new("fresh");
    let pki = pki();
    let ca = dir.write("ca.pem", &pki.ca_pem, 0o644);
    let token = dir.write("token", &format!("{PEER_TOKEN}\n"), 0o600);
    let peer = mock_peer(Some(pki.server), ok_reply(&health_json(&now_rfc3339(-5)))).await;
    let report = poll_one(("ct", format!("https://{}", peer.addr), Some(&ca)), &token).await;
    assert!(report.is_fresh(), "{report:?}");
    assert_eq!(report.instance.as_deref(), Some("mock"));
    assert!(report.age_seconds.unwrap() < 30);
    assert_eq!(report.upstreams[0].state(), "ok");
    let heads = peer.heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 1);
    let head = heads[0].to_ascii_lowercase();
    assert!(head.starts_with("get /v1/health http/1.1"), "{head}");
    assert!(
        head.contains(&format!("authorization: bearer {PEER_TOKEN}")),
        "{head}"
    );
    assert!(
        !head.contains("x-api-key") && !head.contains("cookie"),
        "{head}"
    );
}

#[tokio::test]
async fn stale_data_is_unknown_and_shows_no_counts() {
    let dir = Dir::new("stale");
    let token = dir.write("token", PEER_TOKEN, 0o600);
    let peer = mock_peer(None, ok_reply(&health_json(&now_rfc3339(-3600)))).await;
    let report = poll_one(
        ("ct", format!("http://127.0.0.1:{}", peer.addr.port()), None),
        &token,
    )
    .await;
    assert_eq!(report.state, "unknown");
    assert!(report.upstreams.is_empty());
    assert!(report.error.unwrap().contains("stale"));
}

#[tokio::test]
async fn an_unreachable_peer_is_unknown() {
    let dir = Dir::new("unreachable");
    let token = dir.write("token", PEER_TOKEN, 0o600);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let report = poll_one(("ct", format!("http://127.0.0.1:{port}"), None), &token).await;
    assert_eq!(report.state, "unknown");
    assert!(report.error.unwrap().contains("connect"));
}

#[tokio::test]
async fn a_certificate_from_another_ca_is_refused() {
    let dir = Dir::new("wrong-ca");
    let served = pki();
    let other = pki();
    let ca = dir.write("ca.pem", &other.ca_pem, 0o644);
    let token = dir.write("token", PEER_TOKEN, 0o600);
    let peer = mock_peer(Some(served.server), ok_reply(&health_json(&now_rfc3339(0)))).await;
    let report = poll_one(("ct", format!("https://{}", peer.addr), Some(&ca)), &token).await;
    assert_eq!(report.state, "unknown");
    let error = report.error.unwrap().to_ascii_lowercase();
    assert!(error.contains("certificate"), "{error}");
    assert_eq!(peer.hits.load(Ordering::SeqCst), 0, "no request was sent");
}

#[tokio::test]
async fn a_redirect_is_refused_and_not_followed() {
    let dir = Dir::new("redirect");
    let token = dir.write("token", PEER_TOKEN, 0o600);
    let target = mock_peer(None, ok_reply(&health_json(&now_rfc3339(0)))).await;
    let location = format!("http://127.0.0.1:{}/v1/health", target.addr.port());
    let peer = mock_peer(
        None,
        http_reply("302 Found", &[("location", &location)], ""),
    )
    .await;
    let report = poll_one(
        ("ct", format!("http://127.0.0.1:{}", peer.addr.port()), None),
        &token,
    )
    .await;
    assert_eq!(report.state, "unknown");
    assert!(report.error.unwrap().contains("redirect"));
    assert_eq!(
        target.hits.load(Ordering::SeqCst),
        0,
        "the token went nowhere else"
    );
}

#[tokio::test]
async fn an_oversized_body_is_refused_with_or_without_a_length() {
    let dir = Dir::new("oversized");
    let token = dir.write("token", PEER_TOKEN, 0o600);
    let padding = " ".repeat(70 * 1024);
    let body = format!("{}{padding}", health_json(&now_rfc3339(0)));
    let with_length = mock_peer(None, ok_reply(&body)).await;
    let mut close_delimited =
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n".to_vec();
    close_delimited.extend_from_slice(body.as_bytes());
    let without_length = mock_peer(None, close_delimited).await;
    for peer in [with_length, without_length] {
        let report = poll_one(
            ("ct", format!("http://127.0.0.1:{}", peer.addr.port()), None),
            &token,
        )
        .await;
        assert_eq!(report.state, "unknown");
        assert!(report.error.unwrap().contains("64 KiB"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_token_file_others_can_read_is_refused_before_any_request() {
    let dir = Dir::new("mode");
    let token = dir.write("token", PEER_TOKEN, 0o644);
    let peer = mock_peer(None, ok_reply(&health_json(&now_rfc3339(0)))).await;
    let report = poll_one(
        ("ct", format!("http://127.0.0.1:{}", peer.addr.port()), None),
        &token,
    )
    .await;
    assert_eq!(report.state, "unknown");
    assert!(report.error.unwrap().contains("chmod 600"));
    assert_eq!(peer.hits.load(Ordering::SeqCst), 0);
}

/// A real publisher, served on loopback.
async fn real_publisher() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let text = format!(
        "[upstream.zen]\nbase_url = \"https://example.com/v1\"\n\
         [keys.zen-a]\nupstream = \"zen\"\nfile = \"/unused\"\n\
         [peers]\nenabled = true\npublish = true\nlisten = \"{addr}\"\n\
         peer_token_sha256 = \"{}\"\ninstance = \"real\"\n",
        sha(PEER_TOKEN)
    );
    let config = Config::parse(&text).unwrap();
    let keys: HashMap<String, Secret> = [("zen-a".to_owned(), Secret::new("k"))].into();
    let app = App::new(
        config,
        keys,
        Policy::default(),
        reiki::plugins::PluginChain::default(),
    );
    tokio::spawn(reiki::serve::serve(
        listener,
        reiki::peers::router(app),
        8,
        std::future::pending(),
    ));
    addr
}

#[tokio::test]
async fn a_real_publisher_answers_and_refuses_a_bad_token() {
    let dir = Dir::new("real");
    let addr = real_publisher().await;
    let good = dir.write("good", PEER_TOKEN, 0o600);
    let bad = dir.write("bad", "a-token-the-peer-never-issued", 0o600);
    let url = format!("http://{addr}");
    let report = poll_one(("ct", url.clone(), None), &good).await;
    assert!(report.is_fresh(), "{report:?}");
    assert_eq!(report.instance.as_deref(), Some("real"));
    assert_eq!(report.upstreams[0].healthy_keys, 1);
    let report = poll_one(("ct", url, None), &bad).await;
    assert_eq!(report.state, "unknown");
    assert!(report.error.unwrap().contains("refused the token"));
}

fn reiki_peers(config: &Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
        .arg("--config")
        .arg(config)
        .arg("peers")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn the_command_exits_by_whether_any_peer_answered() {
    let dir = Dir::new("command");
    let addr = real_publisher().await;
    let token = dir.write("token", PEER_TOKEN, 0o600);
    let dead = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let entry = |name: &str, url: String| {
        format!(
            "[peers.{name}]\nurl = \"{url}\"\ntoken_file = \"{}\"\n",
            token.display()
        )
    };
    let only_dead = format!(
        "[peers]\nenabled = true\n{}",
        entry("dead", format!("http://127.0.0.1:{dead}"))
    );
    let config = dir.write("dead.toml", &only_dead, 0o600);
    let (code, out, err) = tokio::task::spawn_blocking(move || reiki_peers(&config, &[]))
        .await
        .unwrap();
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("unknown"), "{out}");
    assert!(err.contains("peer `dead`"), "{err}");

    let mixed = format!("{only_dead}{}", entry("live", format!("http://{addr}")));
    let config = dir.write("mixed.toml", &mixed, 0o600);
    let path = config.clone();
    let (code, out, _) = tokio::task::spawn_blocking(move || reiki_peers(&path, &[]))
        .await
        .unwrap();
    assert_eq!(code, 0, "{out}");
    assert!(
        out.lines()
            .any(|l| l.split_whitespace().take(4).collect::<Vec<_>>()
                == ["live", "real", "zen", "ok"]),
        "{out}"
    );
    let (code, out, _) = tokio::task::spawn_blocking(move || reiki_peers(&config, &["--json"]))
        .await
        .unwrap();
    assert_eq!(code, 0);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["peers"][0]["name"], "dead");
    assert_eq!(doc["peers"][0]["state"], "unknown");
    assert_eq!(doc["peers"][1]["state"], "fresh");

    let none = dir.write("none.toml", "", 0o600);
    let (code, _, err) = tokio::task::spawn_blocking(move || reiki_peers(&none, &[]))
        .await
        .unwrap();
    assert_eq!(code, 2);
    assert!(err.contains("no [peers]"), "{err}");
}
