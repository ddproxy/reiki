//! TLS interception for the tunnel: Reiki answers the handshake for listed hosts with a
//! certificate from a CA the client trusts, then relays each decrypted request like a route.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints, PublicKeyData,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsAcceptor;
use x509_parser::extensions::{GeneralName, GeneralSubtree as ParsedSubtree};

use crate::config::{CaKeyPermissions, InterceptConfig, InterceptHost};
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

/// A fixed description of a CA key parse failure. The parser's own text is deliberately dropped:
/// for example, a malformed PEM header can echo a line of the private key file back to stderr.
fn ca_key_error_kind(error: &rcgen::Error) -> &'static str {
    match error {
        rcgen::Error::CouldNotParseKeyPair => "the key is not a supported private key",
        rcgen::Error::RingKeyRejected(_) => "the crypto provider rejected the key",
        rcgen::Error::PemError(_) => "the key file is not valid PEM",
        rcgen::Error::X509(_) => "the key file holds invalid X.509 data",
        _ => "the key file could not be parsed",
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Whether `host` falls under the name constraint `domain`, where a leading dot admits subdomains only.
fn within(host: &str, domain: &str) -> bool {
    if domain.starts_with('.') {
        host.len() > domain.len() && host.ends_with(domain)
    } else {
        host == domain || host.ends_with(&format!(".{domain}"))
    }
}

fn dns_names(subtrees: Option<&[ParsedSubtree<'_>]>) -> Vec<String> {
    subtrees
        .unwrap_or_default()
        .iter()
        .filter_map(|subtree| match subtree.base {
            GeneralName::DNSName(name) => Some(name.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
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
    cert_path: String,
    key_path: String,
    /// Digest of the two PEM files, so a reload can tell a renewed CA from the one running.
    fingerprint: String,
    /// Unix seconds at which the CA certificate expires.
    not_after: i64,
    /// DNS names the CA may sign for; empty without a DNS name constraint.
    permitted: Vec<String>,
    excluded: Vec<String>,
    key_matches: bool,
}

fn check_key_mode(path: &str, policy: CaKeyPermissions) -> Result<(), String> {
    check_private_key_mode("CA key", path, policy)
}

/// Refuses, or warns about, a private key file (`what`) that its group or other users can read
/// or write.
#[cfg(unix)]
pub(crate) fn check_private_key_mode(
    what: &str,
    path: &str,
    policy: CaKeyPermissions,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot read {path}: {e}"))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 == 0 {
        return Ok(());
    }
    let message = format!("{what} {path} has mode {mode:03o}; it must not be accessible to group or others (chmod 600)");
    match policy {
        CaKeyPermissions::Error => Err(message),
        CaKeyPermissions::Warn => {
            tracing::warn!("{message}");
            Ok(())
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn check_private_key_mode(
    _what: &str,
    _path: &str,
    _policy: CaKeyPermissions,
) -> Result<(), String> {
    Ok(())
}

impl Authority {
    fn load(config: &InterceptConfig) -> Result<Self, String> {
        let (cert_path, key_path) = (config.ca_cert.as_str(), config.ca_key.as_str());
        check_key_mode(key_path, config.ca_key_permissions)?;
        let cert_pem = std::fs::read_to_string(cert_path)
            .map_err(|e| format!("cannot read {cert_path}: {e}"))?;
        let key_pem = std::fs::read_to_string(key_path)
            .map_err(|e| format!("cannot read {key_path}: {e}"))?;
        let fingerprint = crate::auth::sha256_hex(&format!("{cert_pem}\n{key_pem}"));
        let key = KeyPair::from_pem(&key_pem)
            .map_err(|e| format!("bad CA key: {}", ca_key_error_kind(&e)))?;
        let key_spki = key.subject_public_key_info();
        let issuer = Issuer::from_ca_cert_pem(&cert_pem, key)
            .map_err(|e| format!("bad CA certificate: {e}"))?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
            .map_err(|e| format!("bad CA certificate: {e}"))?;
        let cert = pem
            .parse_x509()
            .map_err(|e| format!("bad CA certificate: {e}"))?;
        let (permitted, excluded) = match cert
            .name_constraints()
            .map_err(|e| format!("bad CA certificate: {e}"))?
        {
            Some(ext) => (
                dns_names(ext.value.permitted_subtrees.as_deref()),
                dns_names(ext.value.excluded_subtrees.as_deref()),
            ),
            None => (Vec::new(), Vec::new()),
        };
        Ok(Self {
            issuer,
            cert_path: cert_path.to_owned(),
            key_path: key_path.to_owned(),
            fingerprint,
            not_after: cert.validity().not_after.timestamp(),
            permitted,
            excluded,
            key_matches: cert.public_key().raw == key_spki.as_slice(),
        })
    }

    /// Refuses a CA that cannot serve `hosts` safely.
    fn validate(&self, hosts: &[String]) -> Result<(), String> {
        if !self.key_matches {
            return Err("the CA key does not match the CA certificate".to_owned());
        }
        if self.not_after <= now_unix() {
            return Err("the CA certificate has expired".to_owned());
        }
        if self.permitted.is_empty() {
            return Err("the CA certificate has no DNS name constraint".to_owned());
        }
        for host in hosts {
            let permitted = self.permitted.iter().any(|d| within(host, d));
            if !permitted || self.excluded.iter().any(|d| within(host, d)) {
                return Err(format!("the CA certificate does not cover {host}"));
            }
        }
        Ok(())
    }

    /// The leaf lifetime, kept inside the CA's own remaining validity.
    fn leaf_ttl(&self, ttl: Duration) -> Duration {
        let remaining = u64::try_from(self.not_after.saturating_sub(now_unix())).unwrap_or(0);
        ttl.min(Duration::from_secs(remaining))
    }

    fn server_config(
        &self,
        host: &str,
        ttl: Duration,
    ) -> Result<(Arc<rustls::ServerConfig>, Duration), String> {
        let ttl = self.leaf_ttl(ttl);
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
        Ok((Arc::new(config), ttl))
    }
}

fn configured_hosts(config: &InterceptConfig) -> Vec<String> {
    config.hosts.iter().map(|h| h.host.clone()).collect()
}

/// Days left on a CA that expires at `not_after` (Unix seconds); negative once expired.
pub fn days_left(not_after: i64) -> i64 {
    (not_after - now_unix()).div_euclid(86_400)
}

/// Loads the CA without starting anything, for `reiki check`; returns the days it has left.
pub fn check_ca(config: &InterceptConfig) -> Result<i64, String> {
    let authority = Authority::load(config)?;
    authority.validate(&configured_hosts(config))?;
    Ok(days_left(authority.not_after))
}

/// Days of validity at or below which startup warns.
const CA_WARN_DAYS: i64 = 30;

/// The outcome of checking an intercepted CONNECT's stream token.
#[derive(Debug, PartialEq, Eq)]
pub enum InterceptAuth {
    /// The host entry authorised for this tunnel, pinned against reloads.
    Authorized(InterceptHost),
    /// No credential, or a credential for another stream.
    Refused,
    /// This peer has used up its `[auth]` allowance; wait before trying again.
    Throttled(Duration),
}

/// Serves intercepted CONNECT tunnels.
pub struct Interceptor {
    app: Arc<App>,
    /// The CA and the certificates it signed, replaced together so no old-CA leaf outlives a swap.
    minting: Mutex<Minting>,
    settings: RwLock<Arc<Settings>>,
}

struct Minting {
    authority: Arc<Authority>,
    leaves: HashMap<String, Leaf>,
}

struct Leaf {
    /// Half the certificate's life, after which a new one is minted.
    refresh_at: Instant,
    config: Arc<rustls::ServerConfig>,
}

impl Minting {
    /// Reuses a certificate until half its life is gone, which is always before it expires.
    fn config_for(
        &mut self,
        host: &str,
        ttl: Duration,
        now: Instant,
    ) -> Result<Arc<rustls::ServerConfig>, String> {
        if let Some(leaf) = self.leaves.get(host).filter(|leaf| now < leaf.refresh_at) {
            return Ok(Arc::clone(&leaf.config));
        }
        let (config, life) = self.authority.server_config(host, ttl)?;
        let leaf = Leaf {
            refresh_at: now + life / 2,
            config: Arc::clone(&config),
        };
        self.leaves.insert(host.to_owned(), leaf);
        Ok(config)
    }
}

/// What a reload can replace while tunnels are open.
struct Settings {
    hosts: HashMap<String, InterceptHost>,
    ttl: Duration,
    require_auth: bool,
}

impl Settings {
    fn from_config(config: &InterceptConfig) -> Self {
        Self {
            hosts: config
                .hosts
                .iter()
                .map(|h| (h.host.clone(), h.clone()))
                .collect(),
            ttl: Duration::from_secs(config.leaf_ttl_hours * 3600),
            require_auth: config.require_auth,
        }
    }
}

/// A validated interception change, ready to apply.
pub struct InterceptPlan {
    settings: Arc<Settings>,
    /// The CA to swap in, when its files differ from the running one.
    authority: Option<Arc<Authority>>,
    pub changed: Vec<String>,
}

impl Interceptor {
    pub fn new(app: Arc<App>, config: &InterceptConfig) -> Result<Self, String> {
        let authority = Authority::load(config)?;
        authority.validate(&configured_hosts(config))?;
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
        if !config.require_auth {
            tracing::warn!(
                "interception does not require a stream token on CONNECT; anyone who can reach the tunnel listener can spend the stream keys"
            );
        }
        Ok(Self {
            app,
            minting: Mutex::new(Minting {
                authority: Arc::new(authority),
                leaves: HashMap::new(),
            }),
            settings: RwLock::new(Arc::new(Settings::from_config(config))),
        })
    }

    fn settings(&self) -> Arc<Settings> {
        let guard = self.settings.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }

    /// Checks `config` and describes the change, validating a CA whose paths or contents differ.
    pub fn plan_reload(&self, config: &InterceptConfig) -> Result<InterceptPlan, String> {
        let running = {
            let minting = self.minting.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(&minting.authority)
        };
        let loaded = Authority::load(config)?;
        let current = self.settings();
        let next = Arc::new(Settings::from_config(config));
        let mut changed = Vec::new();
        let ca_changed = loaded.cert_path != running.cert_path
            || loaded.key_path != running.key_path
            || loaded.fingerprint != running.fingerprint;
        let added = next
            .hosts
            .keys()
            .filter(|h| !current.hosts.contains_key(*h))
            .count();
        let hosts: Vec<String> = next.hosts.keys().cloned().collect();
        let authority = if ca_changed {
            loaded.validate(&hosts)?;
            changed.push("intercept ca".to_owned());
            Some(Arc::new(loaded))
        } else {
            // A host added under the same CA must still be one the CA may sign for; fail at
            // reload, not later when a client first connects to it.
            if added > 0 {
                running.validate(&hosts)?;
            }
            None
        };
        let removed = current
            .hosts
            .keys()
            .filter(|h| !next.hosts.contains_key(*h))
            .count();
        let edited = next
            .hosts
            .iter()
            .filter(|(h, v)| current.hosts.get(*h).is_some_and(|old| old != *v))
            .count();
        if added + removed + edited > 0 {
            changed.push(format!(
                "intercept hosts +{added}/-{removed}, {edited} edited"
            ));
        }
        if current.require_auth != next.require_auth {
            changed.push(format!("intercept require_auth {}", next.require_auth));
        }
        if current.ttl != next.ttl {
            changed.push("intercept leaf_ttl_hours".to_owned());
        }
        Ok(InterceptPlan {
            settings: next,
            authority,
            changed,
        })
    }

    /// Swaps in a plan from [`Interceptor::plan_reload`], dropping the certificates the old CA
    /// minted and any leaf whose lifetime the old TTL governed.
    pub fn apply_reload(&self, plan: InterceptPlan) {
        if !plan.settings.require_auth {
            tracing::warn!(
                "interception does not require a stream token on CONNECT; anyone who can reach the tunnel listener can spend the stream keys"
            );
        }
        let ttl_changed = self.settings().ttl != plan.settings.ttl;
        let authority = plan.authority;
        if authority.is_some() || ttl_changed {
            let mut minting = self.minting.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(authority) = authority {
                self.app.set_ca_not_after(authority.not_after);
                tracing::warn!(
                    "the interception CA changed; clients must trust the new CA before their next connection"
                );
                minting.authority = authority;
            }
            minting.leaves.clear();
        }
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = plan.settings;
    }

    pub fn handles(&self, host: &str) -> bool {
        self.settings().hosts.contains_key(host)
    }

    /// Checks one CONNECT against one settings snapshot and the client login throttle. A host
    /// served from a stream spends that stream's keys, so the client must present the stream's
    /// token; a passthrough host spends none and is not counted. The caller passes the returned
    /// entry to [`Interceptor::serve`], so a reload cannot change the stream that serves an
    /// already-authorised tunnel.
    pub fn authorize(
        &self,
        host: &str,
        proxy_authorization: Option<&str>,
        peer: Option<IpAddr>,
    ) -> InterceptAuth {
        let settings = self.settings();
        let Some(entry) = settings.hosts.get(host) else {
            return InterceptAuth::Refused;
        };
        let Some(stream) = entry.stream.as_deref().filter(|_| settings.require_auth) else {
            return InterceptAuth::Authorized(entry.clone());
        };
        // Check and reserve in one lock, so a burst of bad CONNECTs cannot all pass before any
        // failure is counted. This is the client listener's count, shared with direct requests.
        let logins = self.app.logins();
        let attempt = peer.map(|ip| logins.reserve(ip, Instant::now()));
        if let Some(Err(wait)) = attempt {
            tracing::warn!(%host, ?peer, ?wait, "too many failed tunnel logins, refusing");
            return InterceptAuth::Throttled(wait);
        }
        let token = proxy_authorization
            .and_then(crate::auth::proxy_token)
            .unwrap_or_default();
        if self.app.snapshot().authenticate(&token) == Some(stream) {
            if let Some(ip) = peer {
                logins.succeed(ip);
            }
            InterceptAuth::Authorized(entry.clone())
        } else {
            let failures = attempt.and_then(Result::ok).map(|a| a.failures());
            tracing::warn!(%host, ?peer, ?failures, "tunnel refused an intercepted host without a valid stream token");
            InterceptAuth::Refused
        }
    }

    fn config_for(&self, host: &str) -> Result<Arc<rustls::ServerConfig>, String> {
        let ttl = self.settings().ttl;
        let mut minting = self.minting.lock().unwrap_or_else(PoisonError::into_inner);
        minting.config_for(host, ttl, Instant::now())
    }

    /// Runs the handshake on `io`, then relays requests to `target` until the client closes.
    /// `target` is the entry [`Interceptor::authorize`] approved, not a fresh settings lookup.
    pub async fn serve<S>(&self, host: &str, target: InterceptHost, io: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
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

    #[cfg(unix)]
    #[test]
    fn a_ca_key_open_to_group_or_others_is_refused_unless_warned_about() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("reiki-ca-key-{}", std::process::id()));
        std::fs::write(&path, "k").unwrap();
        let path_str = path.to_str().unwrap();
        for (mode, private) in [
            (0o600, true),
            (0o400, true),
            (0o640, false),
            (0o604, false),
            (0o660, false),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                check_key_mode(path_str, CaKeyPermissions::Error).is_ok(),
                private,
                "mode {mode:o}"
            );
            assert!(check_key_mode(path_str, CaKeyPermissions::Warn).is_ok());
        }
        let message = {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            check_key_mode(path_str, CaKeyPermissions::Error).unwrap_err()
        };
        assert!(
            message.contains("644") && message.contains("chmod 600"),
            "{message}"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(
            check_key_mode(path_str, CaKeyPermissions::Error).is_err(),
            "a missing key is an error"
        );
    }

    #[test]
    fn a_name_constraint_admits_the_name_and_its_subdomains() {
        assert!(within("api.test", "api.test"));
        assert!(within("a.b.test", "b.test"));
        assert!(!within("badb.test", "b.test"));
        assert!(!within("b.test", ".b.test"));
        assert!(within("a.b.test", ".b.test"));
    }

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

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reiki-intercept-unit-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app_for_stream(token: &str) -> Arc<App> {
        let text = format!(
            "[upstream.api]\nbase_url = \"https://example.com/v1\"\n\
             [keys.k]\nupstream = \"api\"\nfile = \"/unused\"\n\
             [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n\
             [streams.other]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
            crate::auth::sha256_hex(token),
            crate::auth::sha256_hex("other-stream-token")
        );
        App::new(
            crate::config::Config::parse(&text).unwrap(),
            [("k".to_owned(), crate::proxy::Secret::new("upstream-secret"))].into(),
            crate::chain::Policy::default(),
            crate::plugins::PluginChain::default(),
        )
    }

    fn app_for_stream_with_auth(token: &str, max_failures: u32) -> Arc<App> {
        let text = format!(
            "[auth]\nmax_failures = {max_failures}\nwindow_secs = 60\n\
             [upstream.api]\nbase_url = \"https://example.com/v1\"\n\
             [keys.k]\nupstream = \"api\"\nfile = \"/unused\"\n\
             [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
            crate::auth::sha256_hex(token)
        );
        App::new(
            crate::config::Config::parse(&text).unwrap(),
            [("k".to_owned(), crate::proxy::Secret::new("upstream-secret"))].into(),
            crate::chain::Policy::default(),
            crate::plugins::PluginChain::default(),
        )
    }

    fn intercept_config(dir: &Path, hosts: &[&str]) -> InterceptConfig {
        InterceptConfig {
            ca_cert: dir.join("ca.pem").display().to_string(),
            ca_key: dir.join("ca.key").display().to_string(),
            leaf_ttl_hours: 24,
            require_auth: true,
            ca_key_permissions: CaKeyPermissions::Warn,
            hosts: hosts
                .iter()
                .map(|host| InterceptHost {
                    host: (*host).to_owned(),
                    upstream: "up".to_owned(),
                    stream: None,
                    paths: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn an_authorised_target_survives_a_reload_before_serve() {
        let dir = scratch("authorised-target");
        let ca = dir.join("ca");
        init_ca(&ca, &["api.test".to_owned()], 30, false).unwrap();
        let mut config = intercept_config(&ca, &["api.test"]);
        config.hosts[0].upstream = "api".to_owned();
        config.hosts[0].stream = Some("s".to_owned());
        let interceptor = Interceptor::new(app_for_stream("tok"), &config).unwrap();

        let peer: IpAddr = "192.0.2.10".parse().unwrap();
        assert_eq!(
            interceptor.authorize("api.test", Some("Bearer wrong"), Some(peer)),
            InterceptAuth::Refused
        );
        assert_eq!(
            interceptor.authorize("api.test", None, Some(peer)),
            InterceptAuth::Refused
        );
        assert_eq!(
            interceptor.authorize("api.test", Some("Bearer other-stream-token"), Some(peer)),
            InterceptAuth::Refused,
            "a valid token for another stream does not authorise this host"
        );
        let InterceptAuth::Authorized(target) =
            interceptor.authorize("api.test", Some("Bearer tok"), Some(peer))
        else {
            panic!("the stream token authorises the host");
        };
        assert_eq!(target.upstream, "api");
        assert_eq!(target.stream.as_deref(), Some("s"));

        let mut next = config.clone();
        next.hosts[0].upstream = "other".to_owned();
        interceptor.apply_reload(interceptor.plan_reload(&next).unwrap());
        assert_eq!(
            target.upstream, "api",
            "the entry handed to serve must be the authorised one"
        );
        assert_eq!(
            interceptor.authorize("api.test", Some("Bearer tok"), Some(peer)),
            InterceptAuth::Authorized(next.hosts[0].clone()),
            "a fresh CONNECT uses the reloaded host"
        );
    }

    #[test]
    fn connect_token_guessing_is_throttled_and_a_good_token_clears_the_count() {
        let dir = scratch("connect-throttle");
        let ca = dir.join("ca");
        init_ca(&ca, &["api.test".to_owned()], 30, false).unwrap();
        let mut config = intercept_config(&ca, &["api.test"]);
        config.hosts[0].upstream = "api".to_owned();
        config.hosts[0].stream = Some("s".to_owned());
        let app = app_for_stream_with_auth("tok", 2);
        let interceptor = Interceptor::new(Arc::clone(&app), &config).unwrap();
        let peer: IpAddr = "192.0.2.7".parse().unwrap();
        let other: IpAddr = "192.0.2.8".parse().unwrap();

        assert_eq!(
            interceptor.authorize("api.test", Some("Bearer wrong"), Some(peer)),
            InterceptAuth::Refused
        );
        assert_eq!(
            interceptor.authorize("api.test", Some("Bearer wrong"), Some(peer)),
            InterceptAuth::Refused
        );
        assert!(matches!(
            interceptor.authorize("api.test", Some("Bearer tok"), Some(peer)),
            InterceptAuth::Throttled(_)
        ));
        assert!(
            app.logins().blocked(peer, Instant::now()).is_some(),
            "the client listener's shared count is used"
        );
        assert!(matches!(
            interceptor.authorize("api.test", Some("Bearer tok"), Some(other)),
            InterceptAuth::Authorized(_)
        ));

        // A successful login clears the address's count, as on the client listener. Allow a
        // second attempt after the wrong token, so the good token can be checked and clear it.
        let interceptor = Interceptor::new(app_for_stream_with_auth("tok", 2), &config).unwrap();
        assert_eq!(
            interceptor.authorize("api.test", Some("Bearer wrong"), Some(peer)),
            InterceptAuth::Refused
        );
        assert!(matches!(
            interceptor.authorize("api.test", Some("Bearer tok"), Some(peer)),
            InterceptAuth::Authorized(_)
        ));
        assert_eq!(
            interceptor.authorize("api.test", Some("Bearer wrong"), Some(peer)),
            InterceptAuth::Refused,
            "the good token cleared the count"
        );
    }

    fn write_ca(dir: &Path, params: CertificateParams) {
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        write_private(&dir.join("ca.pem"), cert.pem().as_bytes()).unwrap();
        write_private(&dir.join("ca.key"), key.serialize_pem().as_bytes()).unwrap();
    }

    #[test]
    fn a_leaf_is_reused_for_half_its_life_and_then_replaced() {
        let dir = scratch("leaf-cache");
        let ca = dir.join("ca");
        init_ca(&ca, &["api.example.com".to_owned()], 30, false).unwrap();
        let authority =
            Arc::new(Authority::load(&intercept_config(&ca, &["api.example.com"])).unwrap());
        let mut minting = Minting {
            authority,
            leaves: HashMap::new(),
        };
        let ttl = Duration::from_secs(3600);
        let t0 = Instant::now();
        let first = minting.config_for("api.example.com", ttl, t0).unwrap();
        let reused = minting
            .config_for("api.example.com", ttl, t0 + Duration::from_secs(1799))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &reused));
        let renewed = minting
            .config_for("api.example.com", ttl, t0 + Duration::from_secs(1801))
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &renewed));
    }

    #[test]
    fn a_leaf_ttl_change_drops_the_cached_leaves() {
        let dir = scratch("ttl-cache");
        let ca = dir.join("ca");
        init_ca(&ca, &["api.example.com".to_owned()], 30, false).unwrap();
        let mut config = intercept_config(&ca, &["api.example.com"]);
        let app = crate::proxy::App::new(
            crate::config::Config::parse("").unwrap(),
            HashMap::new(),
            crate::chain::Policy::default(),
            crate::plugins::PluginChain::default(),
        );
        let interceptor = Interceptor::new(Arc::clone(&app), &config).unwrap();
        let first = interceptor.config_for("api.example.com").unwrap();

        config.leaf_ttl_hours = 48;
        let plan = interceptor.plan_reload(&config).unwrap();
        assert!(
            plan.changed
                .iter()
                .any(|change| change == "intercept leaf_ttl_hours"),
            "{:?}",
            plan.changed
        );
        interceptor.apply_reload(plan);
        let second = interceptor.config_for("api.example.com").unwrap();
        assert!(
            !Arc::ptr_eq(&first, &second),
            "the cached leaf must be replaced when the TTL changes"
        );
    }

    #[test]
    fn a_ca_with_a_key_from_another_ca_is_refused_by_check() {
        let dir = scratch("mismatch");
        let a = dir.join("a");
        let b = dir.join("b");
        init_ca(&a, &["api.example.com".to_owned()], 30, false).unwrap();
        init_ca(&b, &["api.example.com".to_owned()], 30, false).unwrap();
        let mixed = dir.join("mixed");
        std::fs::create_dir_all(&mixed).unwrap();
        std::fs::copy(a.join("ca.pem"), mixed.join("ca.pem")).unwrap();
        std::fs::copy(b.join("ca.key"), mixed.join("ca.key")).unwrap();

        let error = check_ca(&intercept_config(&mixed, &["api.example.com"])).unwrap_err();
        assert!(
            error.contains("does not match"),
            "the CA key must match its certificate"
        );
    }

    #[test]
    fn a_ca_without_a_name_constraint_is_refused_by_check() {
        let dir = scratch("unconstrained");
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.not_before = at(BACKDATE, false);
        params.not_after = at(Duration::from_secs(30 * 86_400), true);
        write_ca(&dir, params);

        let error = check_ca(&intercept_config(&dir, &["api.example.com"])).unwrap_err();
        assert!(
            error.contains("name constraint"),
            "the CA needs a DNS name constraint"
        );
    }

    #[test]
    fn an_expired_ca_is_refused_by_check() {
        let dir = scratch("expired");
        let mut params = CertificateParams::new(vec!["api.example.com".to_owned()]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: vec![GeneralSubtree::DnsName("api.example.com".to_owned())],
            excluded_subtrees: Vec::new(),
        });
        params.not_before = at(Duration::from_secs(10 * 86_400), false);
        params.not_after = at(Duration::from_secs(86_400), false);
        write_ca(&dir, params);

        let error = check_ca(&intercept_config(&dir, &["api.example.com"])).unwrap_err();
        assert!(error.contains("expired"), "the CA must not be expired");
    }

    #[test]
    fn a_bad_ca_key_error_never_echoes_the_key_file() {
        let dir = scratch("bad-key-message");
        init_ca(&dir, &["api.example.com".to_owned()], 30, false).unwrap();
        // A malformed PEM header makes the parser's error include this line of the key file.
        let marker = "ca-key-marker-must-not-be-printed";
        let key_pem =
            format!("-----BEGIN PRIVATE KEY-----\n{marker}\n\nAAAA\n-----END PRIVATE KEY-----\n");
        std::fs::remove_file(dir.join("ca.key")).unwrap();
        write_private(&dir.join("ca.key"), key_pem.as_bytes()).unwrap();
        assert!(
            rcgen::KeyPair::from_pem(&key_pem)
                .unwrap_err()
                .to_string()
                .contains(marker),
            "the parser echoes a malformed header; the check must not"
        );

        let error = check_ca(&intercept_config(&dir, &["api.example.com"])).unwrap_err();
        assert!(
            error.contains("bad CA key"),
            "the CA key check must report the fixed bad-CA-key error kind"
        );
        assert!(
            !error.contains(marker),
            "the CA key error must not echo the key file"
        );
    }

    #[test]
    fn ca_init_refuses_a_lifetime_of_zero_days() {
        let dir = scratch("zero-days");
        let error =
            init_ca(&dir.join("ca"), &["api.example.com".to_owned()], 0, false).unwrap_err();
        assert!(error.contains("days"), "{error}");
    }
}
