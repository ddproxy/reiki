//! Optional HTTPS on the peer listener: a certificate chain and key from PEM files, any CA, and a
//! swap on reload when only the files change.

use std::sync::Arc;

use rustls::pki_types::PrivateSec1KeyDer;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer};

use crate::config::PeersConfig;

/// A loaded certificate and key, ready to serve.
pub(crate) struct Loaded {
    pub(crate) config: Arc<rustls::ServerConfig>,
    /// Unix seconds at which the leaf certificate expires.
    pub(crate) not_after: i64,
    /// Digest of both files, so a reload can tell new files from the running ones.
    pub(crate) fingerprint: String,
}

/// Loads `tls_cert` and `tls_key`, or `None` when the listener serves plain HTTP.
pub(crate) fn load(peers: &PeersConfig) -> Result<Option<Loaded>, String> {
    let Some((cert_path, key_path)) = peers.tls_files() else {
        return Ok(None);
    };
    crate::intercept::check_private_key_mode("peer TLS key", key_path, peers.tls_key_permissions)?;
    let cert_pem = std::fs::read(cert_path).map_err(|e| format!("cannot read {cert_path}: {e}"))?;
    let key_pem = std::fs::read(key_path).map_err(|e| format!("cannot read {key_path}: {e}"))?;
    let mut chain = Vec::new();
    for pem in x509_parser::pem::Pem::iter_from_buffer(&cert_pem) {
        let pem = pem.map_err(|e| format!("bad tls_cert {cert_path}: {e}"))?;
        if pem.label == "CERTIFICATE" {
            chain.push(CertificateDer::from(pem.contents));
        }
    }
    let Some(leaf) = chain.first() else {
        return Err(format!("tls_cert {cert_path} holds no PEM certificate"));
    };
    let not_after = x509_parser::parse_x509_certificate(leaf)
        .map_err(|e| format!("bad tls_cert {cert_path}: {e}"))?
        .1
        .validity()
        .not_after
        .timestamp();
    if not_after <= crate::intercept::now_unix() {
        return Err(format!("tls_cert {cert_path} has expired"));
    }
    let key = private_key(&key_pem).ok_or_else(|| {
        format!("tls_key {key_path} holds no PEM private key (PKCS#8, PKCS#1 or SEC1)")
    })?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    // rustls reports a fixed description here; do not interpolate the parsed key material.
    .map_err(|_| "cannot use tls_cert with tls_key".to_owned())?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut digest = cert_pem;
    digest.push(b'\n');
    digest.extend_from_slice(&key_pem);
    Ok(Some(Loaded {
        config: Arc::new(config),
        not_after,
        fingerprint: crate::auth::sha256_hex(&String::from_utf8_lossy(&digest)),
    }))
}

fn private_key(pem: &[u8]) -> Option<PrivateKeyDer<'static>> {
    x509_parser::pem::Pem::iter_from_buffer(pem)
        .filter_map(Result::ok)
        .find_map(|pem| match pem.label.as_str() {
            "PRIVATE KEY" => Some(PrivatePkcs8KeyDer::from(pem.contents).into()),
            "RSA PRIVATE KEY" => Some(PrivatePkcs1KeyDer::from(pem.contents).into()),
            "EC PRIVATE KEY" => Some(PrivateSec1KeyDer::from(pem.contents).into()),
            _ => None,
        })
}

/// Loads the files without serving, for `reiki check`; the days the certificate has left, or
/// `None` without TLS.
pub fn check(peers: &PeersConfig) -> Result<Option<i64>, String> {
    Ok(load(peers)?.map(|loaded| crate::intercept::days_left(loaded.not_after)))
}

/// Days left on each configured peer CA, in config order. An unreadable or non-certificate file
/// is reported as a warning and left out: a status page must not fail because a CA file went
/// missing. No certificate content or peer token is ever logged.
pub fn check_ca_files(peers: &PeersConfig) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for (name, peer) in &peers.peers {
        let Some(path) = peer.ca_file.as_deref() else {
            continue;
        };
        let pem = match std::fs::read(path) {
            Ok(pem) => pem,
            Err(error) => {
                tracing::warn!(peer = %name, file = %path, %error, "cannot read peer ca_file");
                continue;
            }
        };
        let not_after = x509_parser::pem::Pem::iter_from_buffer(&pem)
            .filter_map(Result::ok)
            .filter(|pem| pem.label == "CERTIFICATE")
            .filter_map(|pem| {
                let (_, cert) = x509_parser::parse_x509_certificate(&pem.contents).ok()?;
                Some(cert.validity().not_after.timestamp())
            })
            .min();
        let Some(not_after) = not_after else {
            tracing::warn!(peer = %name, file = %path, "peer ca_file holds no PEM certificate");
            continue;
        };
        let days = crate::intercept::days_left(not_after);
        if days <= 30 {
            tracing::warn!(peer = %name, file = %path, days, "peer CA certificate expires soon");
        }
        out.push((path.to_owned(), days));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = CaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriter(self.0.clone())
        }
    }

    #[test]
    fn ca_files_are_reported_for_status_and_bad_files_are_warned_about() {
        let dir = std::env::temp_dir().join(format!(
            "reiki-peer-ca-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.pem");
        let bad = dir.join("bad.pem");
        let missing = dir.join("missing.pem");
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["peer.example".to_owned()]).unwrap();
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);
        let cert = params.self_signed(&key).unwrap();
        std::fs::write(&good, cert.pem()).unwrap();
        std::fs::write(&bad, "not a certificate\n").unwrap();

        let text = format!(
            "[upstream.zen]\nbase_url = \"https://example.invalid/v1\"\n\
             [peers.good]\nurl = \"https://good.example\"\ntoken_file = \"/t\"\nca_file = \"{}\"\n\
             [peers.bad]\nurl = \"https://bad.example\"\ntoken_file = \"/t\"\nca_file = \"{}\"\n\
             [peers.missing]\nurl = \"https://missing.example\"\ntoken_file = \"/t\"\nca_file = \"{}\"\n",
            good.display(),
            bad.display(),
            missing.display()
        );
        let peers = crate::config::Config::parse(&text).unwrap().peers.unwrap();

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(buffer.clone()))
            .without_time()
            .finish();
        let entries = tracing::subscriber::with_default(subscriber, || check_ca_files(&peers));

        assert_eq!(
            entries.len(),
            1,
            "only the readable certificate is published"
        );
        assert_eq!(entries[0].0, good.to_string_lossy());
        assert!(
            entries[0].1 > 300,
            "a year-long certificate is not near expiry"
        );
        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("peer ca_file holds no PEM certificate"),
            "{logs}"
        );
        assert!(logs.contains("cannot read peer ca_file"), "{logs}");
        assert!(
            !logs.contains("not a certificate"),
            "file content was logged: {logs}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
