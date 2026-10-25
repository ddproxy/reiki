//! A CONNECT-only forward proxy. Bytes are copied in both directions without being read,
//! so the client's own TLS session reaches the destination untouched.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpListener, TcpStream};

use crate::config::TunnelConfig;

const MAX_REQUEST_HEAD: usize = 8 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

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

pub struct Tunnel {
    rules: Vec<Rule>,
    connect_timeout: Duration,
    allow_loopback: bool,
}

impl Tunnel {
    pub fn new(config: &TunnelConfig) -> Self {
        Self {
            rules: config.allow.iter().filter_map(|r| Rule::parse(r)).collect(),
            connect_timeout: Duration::from_secs(config.connect_timeout_secs),
            allow_loopback: config.allow_loopback,
        }
    }

    fn allows(&self, host: &str, port: u16) -> bool {
        self.rules.iter().any(|r| r.allows(host, port))
    }

    /// Keeps an allowed name from being pointed back at this machine's own services.
    fn reachable(&self, addr: &SocketAddr) -> bool {
        if self.allow_loopback {
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
                    || embedded.is_some_and(local_v4)
            }
        };
        !local
    }
}

fn local_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
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
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tracing::warn!(%peer, "tunnel is at its connection limit, dropping a connection");
            continue;
        };
        let tunnel = Arc::clone(&tunnel);
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(error) = handle(stream, &tunnel).await {
                tracing::debug!(%peer, %error, "tunnel connection ended with an error");
            }
        });
    }
}

async fn handle(mut client: TcpStream, tunnel: &Tunnel) -> std::io::Result<()> {
    let Some((target, rest)) = read_connect(&mut client).await? else {
        return respond(&mut client, "400 Bad Request", "expected a CONNECT request").await;
    };
    let Some((host, port)) = split_target(&target) else {
        return respond(&mut client, "400 Bad Request", "bad CONNECT target").await;
    };
    if !tunnel.allows(&host, port) {
        tracing::warn!(%host, port, "tunnel refused a destination that is not allowed");
        return respond(&mut client, "403 Forbidden", "destination not allowed").await;
    }

    let started = Instant::now();
    let upstream = tokio::time::timeout(tunnel.connect_timeout, connect(tunnel, &host, port)).await;
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
    let (up, down) = match copy_bidirectional(&mut client, &mut upstream).await {
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

/// Reads the request head. Returns the CONNECT target and any bytes that arrived after the head.
async fn read_connect(client: &mut TcpStream) -> std::io::Result<Option<(String, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_REQUEST_HEAD {
            return Ok(None);
        }
        let n = match tokio::time::timeout(HEAD_TIMEOUT, client.read(&mut chunk)).await {
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
            Ok(Some((target.to_owned(), buf[end..].to_vec())))
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
        Tunnel {
            rules: allow.iter().map(|r| Rule::parse(r).unwrap()).collect(),
            connect_timeout: Duration::from_secs(5),
            allow_loopback: false,
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
}
