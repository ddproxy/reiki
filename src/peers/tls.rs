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
    .map_err(|e| format!("cannot use tls_cert with tls_key: {e}"))?;
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
