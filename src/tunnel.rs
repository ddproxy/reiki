//! A CONNECT-only forward proxy. Bytes are copied in both directions without being read,
//! so the client's own TLS session reaches the destination untouched.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpListener, TcpStream};

use crate::config::TunnelConfig;
use crate::intercept::{InterceptPlan, Interceptor, Prefixed};

const MAX_REQUEST_HEAD: usize = 8 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// An opaque tunnel is closed after this long, so idle ones cannot hold a slot forever.
const MAX_TUNNEL_LIFETIME: Duration = Duration::from_secs(3600);
/// Most tunnels one peer address may hold open at once.
const MAX_TUNNELS_PER_PEER: usize = 64;

/// One entry of the destination allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    host: Host,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    Any,
    Exact(String),
    /// `*.example.com`: any subdomain, not the apex.
    Suffix(String),
}

impl Rule {
    /// `host`, `host:port`, `*.host` or `*`; the port defaults to 443.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_ascii_lowercase();
        let (host, port) = match text.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (text.as_str(), 443),
        };
        let host = match host {
            "" => return None,
            "*" => Host::Any,
            h if h.starts_with("*.") && h.len() > 2 => Host::Suffix(h[1..].to_owned()),
            h if h.contains('*') => return None,
            h => Host::Exact(h.to_owned()),
        };
        Some(Self { host, port })
    }

    fn allows(&self, host: &str, port: u16) -> bool {
        self.port == port
            && match &self.host {
                Host::Any => true,
                Host::Exact(h) => h == host,
                Host::Suffix(suffix) => host.len() > suffix.len() && host.ends_with(suffix),
            }
    }
}

/// Checks an allowlist entry without building the tunnel.
pub fn valid_rule(text: &str) -> bool {
    Rule::parse(text).is_some()
}

/// True when the allowlist `entries` permit `host:port`.
pub fn allowed(entries: &[String], host: &str, port: u16) -> bool {
    entries
        .iter()
        .filter_map(|e| Rule::parse(e))
        .any(|r| r.allows(host, port))
}

/// The part of the tunnel policy a reload can replace while connections are open.
struct Policy {
    rules: Vec<Rule>,
    /// The `allow` entries as written, to describe what a reload changed.
    allow: Vec<String>,
    connect_timeout: Duration,
    allow_loopback: bool,
    deny_private: bool,
}

impl Policy {
    fn from_config(config: &TunnelConfig) -> Self {
        Self {
            rules: config.allow.iter().filter_map(|r| Rule::parse(r)).collect(),
            allow: config.allow.clone(),
            connect_timeout: Duration::from_secs(config.connect_timeout_secs),
            allow_loopback: config.allow_loopback,
            deny_private: config.deny_private,
        }
    }
}

pub struct Tunnel {
    policy: RwLock<Arc<Policy>>,
    interceptor: Option<Arc<Interceptor>>,
}

/// A validated change to the tunnel, ready to apply without any further failure.
pub struct TunnelPlan {
    policy: Arc<Policy>,
    intercept: Option<InterceptPlan>,
    /// What changes, such as `tunnel allow +2/-0`.
    pub changed: Vec<String>,
    /// Settings that differ from the running tunnel but cannot be applied live.
    pub restart_needed: Vec<String>,
}

impl Tunnel {
    pub fn new(config: &TunnelConfig) -> Self {
        Self {
            policy: RwLock::new(Arc::new(Policy::from_config(config))),
            interceptor: None,
        }
    }

    /// Answers CONNECT requests for the interceptor's hosts itself instead of copying bytes.
    #[must_use]
    pub fn with_interceptor(mut self, interceptor: Arc<Interceptor>) -> Self {
        self.interceptor = Some(interceptor);
        self
    }

    fn policy(&self) -> Arc<Policy> {
        let guard = self.policy.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }

    /// Checks `config` against the running tunnel and describes what a reload would change. The
    /// CA files and the listen address are not reloadable: a CA path change refuses the whole
    /// reload, and adding or removing interception is reported as needing a restart.
    pub fn plan_reload(&self, config: &TunnelConfig) -> Result<TunnelPlan, String> {
        let current = self.policy();
        let next = Arc::new(Policy::from_config(config));
        let mut changed = Vec::new();
        let added = next
            .allow
            .iter()
            .filter(|a| !current.allow.contains(a))
            .count();
        let removed = current
            .allow
            .iter()
            .filter(|a| !next.allow.contains(a))
            .count();
        if added + removed > 0 {
            changed.push(format!("tunnel allow +{added}/-{removed}"));
        }
        if current.deny_private != next.deny_private {
            changed.push(format!("tunnel deny_private {}", next.deny_private));
        }
        if current.allow_loopback != next.allow_loopback {
            changed.push(format!("tunnel allow_loopback {}", next.allow_loopback));
        }
        if current.connect_timeout != next.connect_timeout {
            changed.push("tunnel connect_timeout_secs".to_owned());
        }
        let mut restart_needed = Vec::new();
        let intercept = match (&self.interceptor, &config.intercept) {
            (Some(running), Some(wanted)) => {
                let plan = running.plan_reload(wanted)?;
                changed.extend(plan.changed.iter().cloned());
                Some(plan)
            }
            (None, None) => None,
            (Some(_), None) => {
                restart_needed.push("tunnel.intercept removed".to_owned());
                None
            }
            (None, Some(_)) => {
                restart_needed.push("tunnel.intercept added".to_owned());
                None
            }
        };
        Ok(TunnelPlan {
            policy: next,
            intercept,
            changed,
            restart_needed,
        })
    }

    /// Swaps in a plan from [`Tunnel::plan_reload`]. Open tunnels keep running.
    pub fn apply_reload(&self, plan: TunnelPlan) {
        *self.policy.write().unwrap_or_else(PoisonError::into_inner) = plan.policy;
        if let (Some(interceptor), Some(plan)) = (&self.interceptor, plan.intercept) {
            interceptor.apply_reload(plan);
        }
    }

    fn allows(&self, host: &str, port: u16) -> bool {
        self.policy().rules.iter().any(|r| r.allows(host, port))
    }

    /// Keeps an allowed name from being pointed back at this machine's own services.
    fn reachable(&self, addr: &SocketAddr) -> bool {
        let policy = self.policy();
        if policy.deny_private && private_ip(addr.ip()) {
            return false;
        }
        if policy.allow_loopback {
            return true;
        }
        let local = match addr.ip() {
            IpAddr::V4(v4) => local_v4(v4),
            IpAddr::V6(v6) => {
                let seg = v6.segments();
                let embedded = if seg[..5] == [0; 5] && (seg[5] == 0 || seg[5] == 0xffff) {
                    // IPv4-compatible (::a.b.c.d) and IPv4-mapped (::ffff:a.b.c.d)
                    Some(embedded_v4(&seg))
                } else if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6] == [0; 4] {
                    // NAT64 well-known prefix
                    Some(embedded_v4(&seg))
                } else {
                    None
                };
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (seg[0] & 0xffc0) == 0xfe80
                    || seg == [0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254]
                    || embedded.is_some_and(local_v4)
            }
        };
        !local
    }
}

fn private_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    v4.is_private() || (a == 100 && b & 0xc0 == 64)
}

/// RFC 1918, shared address space and IPv6 unique-local, including IPv4 forms embedded in IPv6.
fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => private_v4(v4),
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            let embedded = (seg[..5] == [0; 5] && (seg[5] == 0 || seg[5] == 0xffff))
                || (seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6] == [0; 4]);
            (seg[0] & 0xfe00) == 0xfc00 || (embedded && private_v4(embedded_v4(&seg)))
        }
    }
}

/// Cloud metadata endpoints outside the link-local range.
fn metadata_v4(v4: std::net::Ipv4Addr) -> bool {
    matches!(v4.octets(), [100, 100, 100, 200] | [168, 63, 129, 16])
}

fn local_v4(v4: std::net::Ipv4Addr) -> bool {
    metadata_v4(v4)
        || v4.is_loopback()
        || v4.is_unspecified()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.octets()[0] == 0
}

fn embedded_v4(seg: &[u16; 8]) -> std::net::Ipv4Addr {
    let [a, b] = seg[6].to_be_bytes();
    let [c, d] = seg[7].to_be_bytes();
    std::net::Ipv4Addr::new(a, b, c, d)
}

/// Most tunnels open at once; further connections are dropped until one closes.
const MAX_TUNNELS: usize = 512;

/// Serves CONNECT requests until the listener fails.
pub async fn serve(listener: TcpListener, tunnel: Arc<Tunnel>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_TUNNELS));
    let peers: Arc<std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>> = Arc::default();
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tracing::warn!(%peer, "tunnel is at its connection limit, refusing a connection");
            refuse(stream);
            continue;
        };
        let Some(guard) = PeerGuard::enter(&peers, peer.ip()) else {
            tracing::warn!(%peer, "peer holds too many tunnels, refusing a connection");
            refuse(stream);
            continue;
        };
        let tunnel = Arc::clone(&tunnel);
        tokio::spawn(async move {
            let _slot = slot;
            let _guard = guard;
            if let Err(error) = handle(stream, &tunnel).await {
                tracing::debug!(%peer, %error, "tunnel connection ended with an error");
            }
        });
    }
}

/// Counts one open tunnel for a peer address and releases it on drop.
struct PeerGuard {
    peers: Arc<std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl PeerGuard {
    fn enter(
        peers: &Arc<std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>>,
        ip: IpAddr,
    ) -> Option<Self> {
        let mut map = peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = map.entry(ip).or_insert(0);
        if *count >= MAX_TUNNELS_PER_PEER {
            return None;
        }
        *count += 1;
        Some(Self {
            peers: Arc::clone(peers),
            ip,
        })
    }
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        let mut map = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = map.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// Answers a refused connection with a 503 without holding up the accept loop.
fn refuse(mut stream: TcpStream) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            respond(&mut stream, "503 Service Unavailable", "too many tunnels"),
        )
        .await;
    });
}

async fn handle(mut client: TcpStream, tunnel: &Tunnel) -> std::io::Result<()> {
    let Some((target, rest, proxy_authorization)) = read_connect(&mut client).await? else {
        return respond(&mut client, "400 Bad Request", "expected a CONNECT request").await;
    };
    let Some((host, port)) = split_target(&target) else {
        return respond(&mut client, "400 Bad Request", "bad CONNECT target").await;
    };
    if !tunnel.allows(&host, port) {
        tracing::warn!(%host, port, "tunnel refused a destination that is not allowed");
        return respond(&mut client, "403 Forbidden", "destination not allowed").await;
    }

    if let Some(interceptor) = tunnel
        .interceptor
        .as_ref()
        .filter(|i| port == 443 && i.handles(&host))
    {
        if !interceptor.authorize(&host, proxy_authorization.as_deref()) {
            tracing::warn!(%host, "tunnel refused an intercepted host without a valid stream token");
            let message = "a stream token is required";
            let response = format!(
                "HTTP/1.1 407 Proxy Authentication Required\r\nproxy-authenticate: Basic realm=\"reiki\"\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{message}",
                message.len()
            );
            client.write_all(response.as_bytes()).await?;
            return client.shutdown().await;
        }
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        tracing::info!(%host, "tunnel intercepting");
        interceptor.serve(&host, Prefixed::new(rest, client)).await;
        return Ok(());
    }

    let started = Instant::now();
    let upstream = tokio::time::timeout(
        tunnel.policy().connect_timeout,
        connect(tunnel, &host, port),
    )
    .await;
    let mut upstream = match upstream {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::warn!(%host, port, %error, "tunnel could not connect");
            return respond(&mut client, "502 Bad Gateway", "could not connect").await;
        }
        Err(_) => {
            tracing::warn!(%host, port, "tunnel connect timed out");
            return respond(&mut client, "504 Gateway Timeout", "connect timed out").await;
        }
    };

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    if !rest.is_empty() {
        upstream.write_all(&rest).await?;
    }
    let copied = tokio::time::timeout(
        MAX_TUNNEL_LIFETIME,
        copy_bidirectional(&mut client, &mut upstream),
    )
    .await
    .unwrap_or_else(|_| Err(std::io::Error::other("tunnel lifetime reached")));
    let (up, down) = match copied {
        Ok(counts) => counts,
        Err(error) => {
            tracing::debug!(%host, port, %error, "tunnel copy ended with an error; byte counts are unknown");
            (0, 0)
        }
    };
    tracing::info!(
        %host,
        port,
        bytes_up = up + rest.len() as u64,
        bytes_down = down,
        millis = started.elapsed().as_millis() as u64,
        "tunnel closed"
    );
    Ok(())
}

async fn connect(tunnel: &Tunnel, host: &str, port: u16) -> std::io::Result<TcpStream> {
    let mut last = None;
    for addr in lookup_host((host, port)).await? {
        if !tunnel.reachable(&addr) {
            last = Some(std::io::Error::other(
                "destination resolves to a local address",
            ));
            continue;
        }
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no address for host")))
}

async fn respond(client: &mut TcpStream, status: &str, message: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{message}",
        message.len()
    );
    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

/// The CONNECT target, the bytes that arrived after the head, and its `Proxy-Authorization`.
type ConnectHead = (String, Vec<u8>, Option<String>);

/// Reads the request head. Returns the CONNECT target and any bytes that arrived after the head.
async fn read_connect(client: &mut TcpStream) -> std::io::Result<Option<ConnectHead>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + HEAD_TIMEOUT;
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_REQUEST_HEAD {
            return Ok(None);
        }
        let n = match tokio::time::timeout_at(deadline, client.read(&mut chunk)).await {
            Ok(result) => result?,
            Err(_) => return Ok(None),
        };
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]);
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(target), Some(version))
            if method.eq_ignore_ascii_case("CONNECT") && version.starts_with("HTTP/1.") =>
        {
            let proxy_authorization = head.lines().skip(1).find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("proxy-authorization")
                    .then(|| value.trim().to_owned())
            });
            Ok(Some((
                target.to_owned(),
                buf[end..].to_vec(),
                proxy_authorization,
            )))
        }
        _ => Ok(None),
    }
}

/// `host:port` with an optional bracketed IPv6 host, lowercased.
fn split_target(target: &str) -> Option<(String, u16)> {
    let (host, port) = target.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), port.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel(allow: &[&str]) -> Tunnel {
        tunnel_with(allow, false)
    }

    fn tunnel_with(allow: &[&str], deny_private: bool) -> Tunnel {
        Tunnel {
            policy: RwLock::new(Arc::new(Policy {
                rules: allow.iter().map(|r| Rule::parse(r).unwrap()).collect(),
                allow: allow.iter().map(|r| (*r).to_owned()).collect(),
                connect_timeout: Duration::from_secs(5),
                allow_loopback: false,
                deny_private,
            })),
            interceptor: None,
        }
    }

    #[test]
    fn rules_match_exact_names_subdomains_and_ports() {
        let t = tunnel(&["opencode.ai", "*.example.com", "alt.test:8443"]);
        assert!(t.allows("opencode.ai", 443));
        assert!(!t.allows("opencode.ai", 80), "port defaults to 443 only");
        assert!(!t.allows("evil-opencode.ai", 443));
        assert!(t.allows("api.example.com", 443));
        assert!(!t.allows("example.com", 443), "the apex is not a subdomain");
        assert!(!t.allows("badexample.com", 443));
        assert!(t.allows("alt.test", 8443) && !t.allows("alt.test", 443));
        assert!(tunnel(&["*"]).allows("anything.example", 443));
        assert!(!tunnel(&[]).allows("opencode.ai", 443));
    }

    #[test]
    fn bad_rules_are_rejected() {
        for bad in ["", "*.", "a*b.com", "host:notaport", ":443"] {
            assert!(!valid_rule(bad), "{bad}");
        }
        assert!(valid_rule("Opencode.AI") && valid_rule("*.x.io:8443") && valid_rule("*"));
    }

    #[test]
    fn targets_split_into_host_and_port() {
        assert_eq!(
            split_target("OpenCode.ai:443"),
            Some(("opencode.ai".into(), 443))
        );
        assert_eq!(split_target("[::1]:443"), Some(("::1".into(), 443)));
        assert_eq!(split_target("nohost"), None);
        assert_eq!(split_target(":443"), None);
        assert_eq!(split_target("host:99999"), None);
    }

    #[test]
    fn local_addresses_are_not_reachable() {
        let t = tunnel(&["*"]);
        for addr in [
            "127.0.0.1:1",
            "0.0.0.0:1",
            "169.254.1.1:1",
            "[::1]:1",
            "[fe80::1]:1",
            "[::ffff:127.0.0.1]:1",
            "0.1.2.3:1",
            "[::127.0.0.1]:1",
            "[64:ff9b::7f00:1]:1",
        ] {
            assert!(!t.reachable(&addr.parse().unwrap()), "{addr}");
        }
        for addr in [
            "93.184.216.34:443",
            "10.0.0.5:443",
            "[2606:4700::1]:443",
            "[64:ff9b::5db8:d822]:443",
        ] {
            assert!(t.reachable(&addr.parse().unwrap()), "{addr}");
        }
    }

    #[test]
    fn cloud_metadata_addresses_outside_link_local_are_unreachable() {
        let t = tunnel(&["*"]);
        for ip in [
            "100.100.100.200",
            "168.63.129.16",
            "169.254.169.254",
            "fd00:ec2::254",
        ] {
            let addr = SocketAddr::new(ip.parse().unwrap(), 443);
            assert!(!t.reachable(&addr), "{ip}");
        }
        let public = SocketAddr::new("93.184.216.34".parse().unwrap(), 443);
        assert!(t.reachable(&public));
    }

    #[test]
    fn deny_private_refuses_the_local_network_but_not_the_internet() {
        let open = tunnel(&["*"]);
        let closed = tunnel_with(&["*"], true);
        let private = [
            "10.0.0.5:443",
            "172.16.1.1:443",
            "192.168.1.10:443",
            "100.64.0.1:443",
            "[fd12::1]:443",
            "[::ffff:10.0.0.5]:443",
        ];
        for addr in private {
            assert!(
                open.reachable(&addr.parse().unwrap()),
                "open by default: {addr}"
            );
        }
        for addr in private {
            assert!(!closed.reachable(&addr.parse().unwrap()), "{addr}");
        }
        for addr in [
            "93.184.216.34:443",
            "[2606:4700::1]:443",
            "172.32.0.1:443",
            "100.128.0.1:443",
        ] {
            assert!(closed.reachable(&addr.parse().unwrap()), "{addr}");
        }
    }
}
