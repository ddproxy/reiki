//! The HTTP listener for the client and admin sockets.
//!
//! `axum::serve` gives hyper no timer, so a client that never finishes its headers holds a
//! connection open forever. This loop adds a header-read timeout and a cap on open connections.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_service::Service;

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

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

/// [`serve`] with an explicit header-read timeout.
pub async fn serve_with(
    listener: TcpListener,
    router: Router,
    max_connections: usize,
    header_read_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) {
    let slots = Arc::new(Semaphore::new(max_connections));
    tokio::pin!(shutdown);
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
            tracing::warn!(%peer, "connection limit reached, dropping a connection");
            continue;
        };
        let router = router.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let service =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let mut router = router.clone();
                    let mut req = req.map(Body::new);
                    req.extensions_mut().insert(crate::auth::Peer(peer.ip()));
                    async move { router.call(req).await }
                });
            let connection = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(header_read_timeout)
                .serve_connection(TokioIo::new(stream), service);
            if let Err(error) = connection.await {
                tracing::debug!(%peer, %error, "connection ended with an error");
            }
        });
    }
    // Let in-flight requests finish, but not forever.
    let all = u32::try_from(max_connections).unwrap_or(u32::MAX);
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, slots.acquire_many(all)).await;
}
