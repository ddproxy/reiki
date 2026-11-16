//! The HTTP listener for the client, admin and peer sockets.
//!
//! `axum::serve` gives hyper no timer, so a client that never finishes its headers holds a
//! connection open forever. This loop adds a header-read timeout and a cap on open connections.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_service::Service;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

static DRAIN_TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// Sets how long a listener waits for requests in flight after its shutdown future completes.
/// Set once at startup; later calls are ignored.
pub fn set_drain_timeout(timeout: Duration) {
    let _ = DRAIN_TIMEOUT.set(timeout);
}

/// The drain time in force.
pub fn drain_timeout() -> Duration {
    DRAIN_TIMEOUT
        .get()
        .copied()
        .unwrap_or(DEFAULT_DRAIN_TIMEOUT)
}
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A flood of refused connections logs one line per interval, not one per connection.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// Spaces out the connection-limit warning and counts what it left unsaid.
struct DropWarning {
    last: Option<Instant>,
    dropped: u64,
}

impl DropWarning {
    const fn new() -> Self {
        Self {
            last: None,
            dropped: 0,
        }
    }

    /// Notes one dropped connection. Returns how many were dropped since the last report when it
    /// is time to log again.
    fn note(&mut self, now: Instant) -> Option<u64> {
        self.dropped += 1;
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < DROP_WARN_INTERVAL)
        {
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.dropped))
    }
}

/// Open connections from one remote address, so one source cannot hold every global slot. A
/// listener whose clients share one address, such as loopback, can exempt it.
struct AddrLimit {
    limit: usize,
    exempt_loopback: bool,
    active: Mutex<HashMap<IpAddr, usize>>,
}

impl AddrLimit {
    fn new(limit: usize) -> Self {
        Self::with_loopback_exemption(limit, false)
    }

    fn new_exempting_loopback(limit: usize) -> Self {
        Self::with_loopback_exemption(limit, true)
    }

    fn with_loopback_exemption(limit: usize, exempt_loopback: bool) -> Self {
        Self {
            limit,
            exempt_loopback,
            active: Mutex::new(HashMap::new()),
        }
    }

    fn acquire(self: &Arc<Self>, ip: IpAddr) -> Option<AddrSlot> {
        if self.exempt_loopback && ip.to_canonical().is_loopback() {
            return Some(AddrSlot { limit: None, ip });
        }
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let count = active.entry(ip).or_insert(0);
        if *count >= self.limit {
            return None;
        }
        *count += 1;
        Some(AddrSlot {
            limit: Some(Arc::clone(self)),
            ip,
        })
    }

    fn release(&self, ip: IpAddr) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = active.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                active.remove(&ip);
            }
        }
    }
}

/// Holds one address slot until the connection ends; an exempt connection holds none.
struct AddrSlot {
    limit: Option<Arc<AddrLimit>>,
    ip: IpAddr,
}

impl Drop for AddrSlot {
    fn drop(&mut self) {
        if let Some(limit) = &self.limit {
            limit.release(self.ip);
        }
    }
}

/// Open connections allowed on one listener, in total and from each remote address.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionLimits {
    pub max_connections: usize,
    pub max_connections_per_addr: usize,
    /// Leave connections from a loopback address out of the per-address count, because every
    /// local process shares `127.0.0.1`.
    pub exempt_loopback: bool,
}

/// Serves `router` until `shutdown` completes, refusing connections beyond `max_connections`.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    shutdown: impl Future<Output = ()>,
) {
    run(
        listener,
        router,
        max_connections,
        None,
        HEADER_READ_TIMEOUT,
        None,
        shutdown,
    )
    .await;
}

/// [`serve`] with [`ConnectionLimits`], such as a per-address cap that exempts loopback.
pub async fn serve_with_limits(
    listener: TcpListener,
    router: Router,
    limits: ConnectionLimits,
    shutdown: impl Future<Output = ()>,
) {
    let addr_limit = if limits.exempt_loopback {
        AddrLimit::new_exempting_loopback(limits.max_connections_per_addr)
    } else {
        AddrLimit::new(limits.max_connections_per_addr)
    };
    run(
        listener,
        router,
        limits.max_connections,
        Some(Arc::new(addr_limit)),
        HEADER_READ_TIMEOUT,
        None,
        shutdown,
    )
    .await;
}

/// [`serve`] with a cap on open connections from each remote address.
pub async fn serve_per_addr(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    max_connections_per_addr: usize,
    shutdown: impl Future<Output = ()>,
) {
    run(
        listener,
        router,
        max_connections,
        Some(Arc::new(AddrLimit::new(max_connections_per_addr))),
        HEADER_READ_TIMEOUT,
        None,
        shutdown,
    )
    .await;
}

/// The server configuration a TLS listener uses for each new connection, read per connection so
/// a reload can swap it.
pub type TlsSource = Arc<dyn Fn() -> Arc<rustls::ServerConfig> + Send + Sync>;

/// [`serve`] over TLS: every connection first completes a handshake with `tls()`.
pub async fn serve_tls(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    tls: TlsSource,
    shutdown: impl Future<Output = ()>,
) {
    run(
        listener,
        router,
        max_connections,
        None,
        HEADER_READ_TIMEOUT,
        Some(tls),
        shutdown,
    )
    .await;
}

/// [`serve_tls`] with a cap on open connections from each remote address.
pub async fn serve_tls_per_addr(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    max_connections_per_addr: usize,
    tls: TlsSource,
    shutdown: impl Future<Output = ()>,
) {
    run(
        listener,
        router,
        max_connections,
        Some(Arc::new(AddrLimit::new(max_connections_per_addr))),
        HEADER_READ_TIMEOUT,
        Some(tls),
        shutdown,
    )
    .await;
}

/// [`serve`] with an explicit header-read timeout.
pub async fn serve_with(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    header_read_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) {
    run(
        listener,
        router,
        max_connections,
        None,
        header_read_timeout,
        None,
        shutdown,
    )
    .await;
}

async fn run(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    addr_limit: Option<Arc<AddrLimit>>,
    header_read_timeout: Duration,
    tls: Option<TlsSource>,
    shutdown: impl Future<Output = ()>,
) {
    let slots = Arc::new(Semaphore::new(max_connections));
    tokio::pin!(shutdown);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut drops = DropWarning::new();
    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, peer)) = accepted else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let addr_slot = match &addr_limit {
            Some(limit) => match limit.acquire(peer.ip()) {
                Some(slot) => Some(slot),
                None => {
                    tracing::debug!(
                        %peer,
                        "per-address connection limit reached, dropping connection"
                    );
                    continue;
                }
            },
            None => None,
        };
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            if let Some(count) = drops.note(Instant::now()) {
                tracing::warn!(%peer, count, "connection limit reached, dropping connections");
            }
            continue;
        };
        let router = router.clone();
        let tls = tls.as_ref().map(|source| source());
        let stop_rx = stop_rx.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _addr_slot = addr_slot;
            let result = match tls {
                None => connection(stream, router, peer, header_read_timeout, stop_rx).await,
                Some(config) => {
                    let accept = tokio_rustls::TlsAcceptor::from(config).accept(stream);
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, accept).await {
                        Ok(Ok(stream)) => {
                            connection(stream, router, peer, header_read_timeout, stop_rx).await
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(%peer, %error, "TLS handshake failed");
                            return;
                        }
                        Err(_) => {
                            tracing::debug!(%peer, "TLS handshake timed out");
                            return;
                        }
                    }
                }
            };
            if let Err(error) = result {
                tracing::debug!(%peer, %error, "connection ended with an error");
            }
        });
    }
    // Stop listening so new connections are refused, and tell open ones to finish up.
    drop(listener);
    stop_tx.send_replace(true);
    // Let in-flight requests finish, but not forever.
    let all = u32::try_from(max_connections).unwrap_or(u32::MAX);
    let _ = tokio::time::timeout(drain_timeout(), slots.acquire_many(all)).await;
}

async fn connection<IO>(
    io: IO,
    router: Router,
    peer: std::net::SocketAddr,
    header_read_timeout: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), hyper::Error>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mut router = router.clone();
        let mut req = req.map(Body::new);
        req.extensions_mut().insert(crate::auth::Peer(peer.ip()));
        async move { router.call(req).await }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout)
        .serve_connection(TokioIo::new(io), service);
    tokio::pin!(conn);
    // The guard `wait_for` returns is not `Send`, so it is dropped inside this block.
    let stopping = async {
        let _ = stop.wait_for(|stopping| *stopping).await;
    };
    tokio::select! {
        result = conn.as_mut() => result,
        // On shutdown an idle keep-alive connection closes at once and a busy one closes
        // after its current response, so the drain does not wait on idle clients.
        () = stopping => {
            conn.as_mut().graceful_shutdown();
            conn.await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_per_address_limit_counts_open_connections_and_frees_slots() {
        let limit = Arc::new(AddrLimit::new(2));
        let a = "192.0.2.1".parse().unwrap();
        let b = "192.0.2.2".parse().unwrap();
        let first = limit.acquire(a).expect("first slot");
        let second = limit.acquire(a).expect("second slot");
        assert!(limit.acquire(a).is_none(), "the address limit is reached");
        let other = limit.acquire(b).expect("another address has its own slots");
        drop(second);
        let again = limit
            .acquire(a)
            .expect("a closed connection frees its slot");
        drop((first, again, other));
        assert!(limit.acquire(a).is_some(), "every slot returned");
        assert!(limit.acquire(b).is_some(), "and the other address too");
    }

    #[test]
    fn the_per_address_limit_can_exempt_loopback() {
        let limit = Arc::new(AddrLimit::new_exempting_loopback(1));
        let local = "127.0.0.1".parse().unwrap();
        let remote = "192.0.2.1".parse().unwrap();
        let mapped = "::ffff:127.0.0.1".parse().unwrap();
        let _mapped = limit
            .acquire(mapped)
            .expect("an IPv4-mapped loopback address is exempt too");
        let one = limit.acquire(local).expect("loopback is allowed");
        let two = limit.acquire(local).expect("loopback holds no slot");
        let three = limit
            .acquire(remote)
            .expect("a remote address gets its slot");
        assert!(
            limit.acquire(remote).is_none(),
            "the remote address is still capped"
        );
        drop((one, two, three));
        assert!(
            limit.acquire(remote).is_some(),
            "a closed connection frees it"
        );
    }

    #[test]
    fn the_connection_limit_warning_is_spaced_and_counts_the_drops() {
        let mut drops = DropWarning::new();
        let t0 = Instant::now();
        assert_eq!(
            drops.note(t0),
            Some(1),
            "the first drop is reported at once"
        );
        assert_eq!(drops.note(t0 + Duration::from_secs(1)), None);
        assert_eq!(drops.note(t0 + Duration::from_secs(9)), None);
        assert_eq!(
            drops.note(t0 + DROP_WARN_INTERVAL),
            Some(3),
            "the next report counts the silent ones and itself"
        );
        assert_eq!(drops.note(t0 + DROP_WARN_INTERVAL), None);
    }
}
