//! TLS interception for the tunnel: Reiki answers the handshake for listed hosts with a
//! certificate from a CA the client trusts, then relays each decrypted request like a route.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsAcceptor;

use crate::config::{InterceptConfig, InterceptHost};
use crate::proxy::{handle_intercepted, App};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// Tolerates a client clock that runs slightly behind.
const BACKDATE: Duration = Duration::from_secs(300);

/// Only plain names, so a certificate is never minted for a wildcard or an address.
pub fn valid_host(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && !labels
            .last()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
}

/// The lowercase host of an upstream's base URL.
pub fn base_host(base_url: &str) -> Option<String> {
    reqwest::Url::parse(base_url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn at(offset: Duration, future: bool) -> time::OffsetDateTime {
    let now = time::OffsetDateTime::now_utc();
    let offset = time::Duration::seconds(i64::try_from(offset.as_secs()).unwrap_or(i64::MAX / 4));
    if future {
        now + offset
    } else {
        now - offset
    }
}

/// Name-constrained to `hosts`, so a stolen key cannot impersonate anything else.
pub fn init_ca(dir: &Path, hosts: &[String], days: u64, replace: bool) -> Result<(), String> {
    if hosts.is_empty() {
        return Err("name at least one host".to_owned());
    }
    if let Some(bad) = hosts.iter().find(|h| !valid_host(h)) {
        return Err(format!("`{bad}` must be a plain lowercase DNS name"));
    }
    if days == 0 || days > 3650 {
        return Err("days must be between 1 and 3650".to_owned());
    }
    let cert_path = dir.join("ca.pem");
    let key_path = dir.join("ca.key");
    if cert_path.exists() || key_path.exists() {
        if !replace {
            return Err(format!(
                "{} already holds a CA; pass --replace to overwrite it",
                dir.display()
            ));
        }
        for path in [&cert_path, &key_path] {
            if path.exists() {
                std::fs::remove_file(path).map_err(err)?;
            }
        }
    }

    let key = KeyPair::generate().map_err(err)?;
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "Reiki intercept CA");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: hosts.iter().cloned().map(GeneralSubtree::DnsName).collect(),
        excluded_subtrees: Vec::new(),
    });
    params.not_before = at(BACKDATE, false);
    params.not_after = at(Duration::from_secs(days * 86_400), true);
    let cert = params.self_signed(&key).map_err(err)?;

    std::fs::create_dir_all(dir).map_err(err)?;
    std::fs::write(&cert_path, cert.pem()).map_err(err)?;
    write_private(&key_path, key.serialize_pem().as_bytes()).map_err(err)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

struct Authority {
    issuer: Issuer<'static, KeyPair>,
    /// Unix seconds at which the CA certificate expires.
    not_after: i64,
}

impl Authority {
    fn load(cert: &str, key: &str) -> Result<Self, String> {
        let cert_pem =
            std::fs::read_to_string(cert).map_err(|e| format!("cannot read {cert}: {e}"))?;
        let key_pem =
            std::fs::read_to_string(key).map_err(|e| format!("cannot read {key}: {e}"))?;
        let key = KeyPair::from_pem(&key_pem).map_err(|e| format!("bad CA key: {e}"))?;
        let issuer = Issuer::from_ca_cert_pem(&cert_pem, key)
            .map_err(|e| format!("bad CA certificate: {e}"))?;
        let not_after = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
            .map_err(|e| format!("bad CA certificate: {e}"))
            .and_then(|(_, pem)| {
                pem.parse_x509()
                    .map(|cert| cert.validity().not_after.timestamp())
                    .map_err(|e| format!("bad CA certificate: {e}"))
            })?;
        Ok(Self { issuer, not_after })
    }

    fn server_config(
        &self,
        host: &str,
        ttl: Duration,
    ) -> Result<Arc<rustls::ServerConfig>, String> {
        let key = KeyPair::generate().map_err(err)?;
        let mut params = CertificateParams::new(vec![host.to_owned()]).map_err(err)?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, host);
        params.distinguished_name = name;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = at(BACKDATE, false);
        params.not_after = at(ttl, true);
        let cert = params.signed_by(&key, &self.issuer).map_err(err)?;

        let chain = vec![CertificateDer::from(cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(err)?;
        // The upstream leg negotiates HTTP/2 on its own, so the client leg stays HTTP/1.1.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

/// Days left on a CA that expires at `not_after` (Unix seconds); negative once expired.
pub fn days_left(not_after: i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    (not_after - now).div_euclid(86_400)
}

/// Loads the CA without starting anything, for `reiki check`; returns the days it has left.
pub fn check_ca(config: &InterceptConfig) -> Result<i64, String> {
    Authority::load(&config.ca_cert, &config.ca_key).map(|a| days_left(a.not_after))
}

/// Days of validity at or below which startup warns.
const CA_WARN_DAYS: i64 = 30;

/// Serves intercepted CONNECT tunnels.
pub struct Interceptor {
    app: Arc<App>,
    authority: Authority,
    hosts: HashMap<String, InterceptHost>,
    ttl: Duration,
    configs: Mutex<HashMap<String, (Instant, Arc<rustls::ServerConfig>)>>,
}

impl Interceptor {
    pub fn new(app: Arc<App>, config: &InterceptConfig) -> Result<Self, String> {
        let authority = Authority::load(&config.ca_cert, &config.ca_key)?;
        let left = days_left(authority.not_after);
        app.set_ca_not_after(authority.not_after);
        if left < 0 {
            tracing::error!(
                days_overdue = -left,
                "the interception CA has expired; clients will refuse its certificates"
            );
        } else if left <= CA_WARN_DAYS {
            tracing::warn!(days_left = left, "the interception CA expires soon");
        } else {
            tracing::info!(days_left = left, "interception CA loaded");
        }
        Ok(Self {
            app,
            authority,
            hosts: config
                .hosts
                .iter()
                .map(|h| (h.host.clone(), h.clone()))
                .collect(),
            ttl: Duration::from_secs(config.leaf_ttl_hours * 3600),
            configs: Mutex::new(HashMap::new()),
        })
    }

    pub fn handles(&self, host: &str) -> bool {
        self.hosts.contains_key(host)
    }

    /// Reuses a certificate until half its life is gone.
    fn config_for(&self, host: &str) -> Result<Arc<rustls::ServerConfig>, String> {
        let mut cache = self.configs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((made, config)) = cache.get(host) {
            if made.elapsed() < self.ttl / 2 {
                return Ok(Arc::clone(config));
            }
        }
        let config = self.authority.server_config(host, self.ttl)?;
        cache.insert(host.to_owned(), (Instant::now(), Arc::clone(&config)));
        Ok(config)
    }

    /// Runs the handshake on `io`, then relays requests until the client closes.
    pub async fn serve<S>(&self, host: &str, io: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let Some(target) = self.hosts.get(host).cloned() else {
            return;
        };
        let config = match self.config_for(host) {
            Ok(config) => config,
            Err(error) => {
                tracing::error!(%host, %error, "cannot mint a certificate");
                return;
            }
        };
        let accepted =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, TlsAcceptor::from(config).accept(io)).await;
        let tls = match accepted {
            Ok(Ok(tls)) => tls,
            Ok(Err(error)) => {
                tracing::debug!(%host, %error, "intercepted handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!(%host, "intercepted handshake timed out");
                return;
            }
        };

        let app = Arc::clone(&self.app);
        let host_name = host.to_owned();
        let service =
            hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let app = Arc::clone(&app);
                let host = host_name.clone();
                let target = target.clone();
                async move {
                    let req = req.map(axum::body::Body::new);
                    Ok::<_, Infallible>(handle_intercepted(&app, &host, &target, req).await)
                }
            });
        let served = hyper::server::conn::http1::Builder::new()
            .timer(hyper_util::rt::TokioTimer::new())
            .preserve_header_case(true)
            .header_read_timeout(HEADER_TIMEOUT)
            .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
            .await;
        if let Err(error) = served {
            tracing::debug!(%host, %error, "intercepted connection ended with an error");
        }
    }
}

/// A stream that yields bytes already read from it before reading on.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.prefix.len() {
            let take = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + take]);
            self.pos += take;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_dns_names_are_valid_hosts() {
        for good in ["api.example.com", "a-b.example.io", "x1.y2.z3"] {
            assert!(valid_host(good), "{good}");
        }
        for bad in [
            "",
            "localhost",
            "*.example.com",
            "Example.com",
            "a..b",
            "-a.example.com",
            "a-.example.com",
            "127.0.0.1",
            "exa mple.com",
            "example.com:443",
        ] {
            assert!(!valid_host(bad), "{bad}");
        }
    }

    #[test]
    fn the_base_host_is_read_from_the_url() {
        assert_eq!(
            base_host("https://API.Example.com/v1").as_deref(),
            Some("api.example.com")
        );
        assert_eq!(base_host("not a url"), None);
    }

    #[tokio::test]
    async fn prefixed_bytes_come_first() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut near, far) = tokio::io::duplex(64);
        near.write_all(b"tail").await.unwrap();
        drop(near);
        let mut stream = Prefixed::new(b"head".to_vec(), far);
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "headtail");
    }
}
