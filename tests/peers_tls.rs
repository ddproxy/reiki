//! HTTPS on the peer listener: a certificate from any CA, a private key file, a swap on reload,
//! and `reiki peers` pinned to the CA that issued it.
#![cfg(feature = "peers")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::peers::client::{poll, PeerReport};
use reiki::proxy::{App, Secret};

const PEER_TOKEN: &str = "peer-token-for-tls-tests";

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("reiki-peers-tls-{label}-{}", std::process::id()));
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

/// A test CA and a leaf it issued for `127.0.0.1`, as PEM: (CA certificate, leaf chain, leaf key).
fn issue() -> (String, String, String) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    (ca_cert.pem(), leaf.pem(), leaf_key.serialize_pem())
}

struct Files {
    ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
}

fn write_issued(dir: &Dir, label: &str, key_mode: u32) -> Files {
    let (ca, cert, key) = issue();
    Files {
        ca: dir.write(&format!("{label}-ca.pem"), &ca, 0o644),
        cert: dir.write("cert.pem", &cert, 0o644),
        key: dir.write("key.pem", &key, key_mode),
    }
}

fn publisher_text(dir: &Dir, addr: SocketAddr, files: &Files, extra: &str) -> String {
    let key_file = dir.write("zen-a", "k\n", 0o600);
    format!(
        "[upstream.zen]\nbase_url = \"https://example.com/v1\"\n\
         [keys.zen-a]\nupstream = \"zen\"\nfile = \"{}\"\n\
         [peers]\nenabled = true\npublish = true\nlisten = \"{addr}\"\n\
         peer_token_sha256 = \"{}\"\ninstance = \"tls\"\n\
         tls_cert = \"{}\"\ntls_key = \"{}\"\n{extra}",
        key_file.display(),
        reiki::auth::sha256_hex(PEER_TOKEN),
        files.cert.display(),
        files.key.display(),
    )
}

/// Starts a real publisher over HTTPS with the config `text(addr)`, or returns why it would not
/// start. The socket is bound first, so no other test can take its port.
async fn start(
    text: impl FnOnce(SocketAddr) -> String,
) -> Result<(Arc<App>, SocketAddr, String), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let text = text(addr);
    let config = Config::parse(&text).unwrap();
    let keys: HashMap<String, Secret> = [("zen-a".to_owned(), Secret::new("k"))].into();
    let app = App::new(
        config,
        keys,
        Policy::default(),
        reiki::plugins::PluginChain::default(),
    );
    let served = reiki::peers::serve(listener, Arc::clone(&app), 8)?;
    tokio::spawn(served);
    Ok((app, addr, text))
}

async fn read_with(dir: &Dir, addr: SocketAddr, ca: &Path) -> PeerReport {
    let token = dir.write("token", PEER_TOKEN, 0o600);
    let text = format!(
        "[peers]\nenabled = true\n[peers.tls]\nurl = \"https://{addr}\"\n\
         token_file = \"{}\"\nca_file = \"{}\"\n",
        token.display(),
        ca.display()
    );
    let config = Config::parse(&text).unwrap();
    poll(config.peers.as_ref().unwrap()).await.remove(0)
}

#[tokio::test]
async fn the_client_reads_a_tls_listener_through_the_issuing_ca_only() {
    let dir = Dir::new("pinned");
    let files = write_issued(&dir, "one", 0o600);
    let (_app, addr, _) = start(|addr| publisher_text(&dir, addr, &files, ""))
        .await
        .unwrap();
    let report = read_with(&dir, addr, &files.ca).await;
    assert!(report.is_fresh(), "{report:?}");
    assert_eq!(report.instance.as_deref(), Some("tls"));

    let (other_ca, _, _) = issue();
    let other = dir.write("other-ca.pem", &other_ca, 0o644);
    let report = read_with(&dir, addr, &other).await;
    assert_eq!(report.state, "unknown");
    let error = report.error.unwrap().to_ascii_lowercase();
    assert!(error.contains("certificate"), "{error}");
}

#[tokio::test]
async fn a_plain_http_request_to_the_tls_listener_gets_no_answer() {
    let dir = Dir::new("plain");
    let files = write_issued(&dir, "one", 0o600);
    let (_app, addr, _) = start(|addr| publisher_text(&dir, addr, &files, ""))
        .await
        .unwrap();
    let result = reqwest::Client::new()
        .get(format!("http://{addr}/v1/health"))
        .bearer_auth(PEER_TOKEN)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    assert!(
        result.map_or(true, |res| !res.status().is_success()),
        "no health over plain HTTP"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_tls_key_others_can_read_is_refused_unless_warned_about() {
    let dir = Dir::new("mode");
    let files = write_issued(&dir, "one", 0o644);
    let error = start(|addr| publisher_text(&dir, addr, &files, ""))
        .await
        .err()
        .expect("refused");
    assert!(
        error.contains("peer TLS key") && error.contains("chmod 600"),
        "{error}"
    );
    assert!(
        start(|addr| publisher_text(&dir, addr, &files, "tls_key_permissions = \"warn\"\n"))
            .await
            .is_ok(),
        "warn loads it"
    );
}

#[tokio::test]
async fn a_key_that_does_not_match_the_certificate_is_refused() {
    let dir = Dir::new("mismatch");
    let files = write_issued(&dir, "one", 0o600);
    let (_, _, other_key) = issue();
    dir.write("key.pem", &other_key, 0o600);
    let error = start(|addr| publisher_text(&dir, addr, &files, ""))
        .await
        .err()
        .expect("refused");
    assert!(error.contains("tls_cert"), "{error}");
}

#[tokio::test]
async fn a_reload_swaps_the_certificate_without_a_restart() {
    let dir = Dir::new("swap");
    let first = write_issued(&dir, "first", 0o600);
    let (app, addr, text) = start(|addr| publisher_text(&dir, addr, &first, ""))
        .await
        .unwrap();
    let config = dir.write("reiki.toml", &text, 0o600);
    assert!(read_with(&dir, addr, &first.ca).await.is_fresh());

    let unchanged = app.reload_from_file(&config).unwrap();
    assert!(
        !unchanged.changed.iter().any(|c| c == "peers.tls"),
        "{unchanged:?}"
    );

    let second = write_issued(&dir, "second", 0o600);
    let report = app.reload_from_file(&config).unwrap();
    assert!(
        report.changed.iter().any(|c| c == "peers.tls"),
        "{report:?}"
    );
    assert!(report.restart_needed.is_empty(), "{report:?}");
    assert!(read_with(&dir, addr, &second.ca).await.is_fresh());
    assert_eq!(read_with(&dir, addr, &first.ca).await.state, "unknown");

    std::fs::write(&second.cert, "not a certificate").unwrap();
    assert!(
        app.reload_from_file(&config).is_err(),
        "a broken certificate fails the reload"
    );
    assert!(
        read_with(&dir, addr, &second.ca).await.is_fresh(),
        "and the running one stays"
    );

    let plain = text
        .lines()
        .filter(|line| !line.starts_with("tls_"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&config, plain).unwrap();
    let report = app.reload_from_file(&config).unwrap();
    assert!(
        report.restart_needed.iter().any(|r| r == "peers.tls"),
        "{report:?}"
    );
}

#[tokio::test]
async fn check_validates_the_files_and_shows_the_expiry() {
    let dir = Dir::new("check");
    let files = write_issued(&dir, "one", 0o600);
    let text = publisher_text(&dir, "127.0.0.1:4999".parse().unwrap(), &files, "");
    let config = dir.write("reiki.toml", &text, 0o600);
    let check = |config: &Path| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
            .arg("--config")
            .arg(config)
            .arg("check")
            .output()
            .unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };
    let (ok, out) = check(&config);
    assert!(ok, "{out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("peers TLS certificate valid for ")
                && l.ends_with(" more day(s)")),
        "{out}"
    );
    assert!(
        out.contains("publishing on 127.0.0.1:4999 over https"),
        "{out}"
    );
    std::fs::write(&files.cert, "garbage").unwrap();
    let (ok, out) = check(&config);
    assert!(!ok, "{out}");
    assert!(out.contains("tls_cert"), "{out}");
}

#[test]
fn a_non_loopback_listener_needs_tls() {
    let dir = Dir::new("loopback");
    let files = write_issued(&dir, "one", 0o600);
    let text = publisher_text(&dir, "0.0.0.0:4999".parse().unwrap(), &files, "");
    assert!(
        Config::parse(&text).is_ok(),
        "with TLS any address is allowed"
    );
    let plain = text
        .lines()
        .filter(|line| !line.starts_with("tls_"))
        .collect::<Vec<_>>()
        .join("\n");
    let error = Config::parse(&plain).unwrap_err().to_string();
    assert!(
        error.contains("loopback") && error.contains("tls_cert"),
        "{error}"
    );
    let half = text
        .lines()
        .filter(|line| !line.starts_with("tls_key"))
        .collect::<Vec<_>>()
        .join("\n");
    let error = Config::parse(&half).unwrap_err().to_string();
    assert!(error.contains("set both or neither"), "{error}");
}
