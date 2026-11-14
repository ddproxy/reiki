//! The HTTP listener for the client, admin and peer sockets.
//!
//! `axum::serve` gives hyper no timer, so a client that never finishes its headers holds a
//! connection open forever. This loop adds a header-read timeout and a cap on open connections.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_service::Service;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
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

/// Serves `router` until `shutdown` completes, refusing connections beyond `max_connections`.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    shutdown: impl Future<Output = ()>,
) {
    serve_with(
        listener,
        router,
        max_connections,
        HEADER_READ_TIMEOUT,
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
    header_read_timeout: Duration,
    tls: Option<TlsSource>,
    shutdown: impl Future<Output = ()>,
) {
    let slots = Arc::new(Semaphore::new(max_connections));
    tokio::pin!(shutdown);
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
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            if let Some(count) = drops.note(Instant::now()) {
                tracing::warn!(%peer, count, "connection limit reached, dropping connections");
            }
            continue;
        };
        let router = router.clone();
        let tls = tls.as_ref().map(|source| source());
        tokio::spawn(async move {
            let _slot = slot;
            let result = match tls {
                None => connection(stream, router, peer, header_read_timeout).await,
                Some(config) => {
                    let accept = tokio_rustls::TlsAcceptor::from(config).accept(stream);
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, accept).await {
                        Ok(Ok(stream)) => {
                            connection(stream, router, peer, header_read_timeout).await
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
    // Let in-flight requests finish, but not forever.
    let all = u32::try_from(max_connections).unwrap_or(u32::MAX);
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, slots.acquire_many(all)).await;
}

async fn connection<IO>(
    io: IO,
    router: Router,
    peer: std::net::SocketAddr,
    header_read_timeout: Duration,
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
    hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout)
        .serve_connection(TokioIo::new(io), service)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

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
