use std::net::IpAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use reiki::serve::{ConnectionLimits, serve_with, serve_with_limits};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn start(max_connections: usize, header_timeout: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let router = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(serve_with(
        listener,
        router,
        max_connections,
        header_timeout,
        std::future::pending(),
    ));
    addr
}

async fn start_with_caps(listener: TcpListener, max_connections: usize, per_addr: usize) -> String {
    let addr = listener.local_addr().unwrap().to_string();
    let router = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(serve_with_limits(
        listener,
        router,
        ConnectionLimits {
            max_connections,
            max_connections_per_addr: per_addr,
            exempt_loopback: true,
        },
        std::future::pending(),
    ));
    addr
}

/// A local, non-loopback IPv4 address when the host has a route; no packet is sent.
fn non_loopback_local_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// True once the peer has closed the connection, within `wait`.
async fn closed_within(stream: &mut TcpStream, wait: Duration) -> bool {
    let mut buf = [0u8; 64];
    matches!(
        tokio::time::timeout(wait, stream.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

#[tokio::test]
async fn a_client_that_never_finishes_its_headers_is_cut_off() {
    let addr = start(8, Duration::from_millis(300)).await;
    let mut slow = TcpStream::connect(&addr).await.unwrap();
    slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nX-Slow: ")
        .await
        .unwrap();
    assert!(closed_within(&mut slow, Duration::from_secs(3)).await);
}

#[tokio::test]
async fn a_normal_request_is_served() {
    let addr = start(8, Duration::from_secs(5)).await;
    let body = reqwest::get(format!("http://{addr}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn connections_beyond_the_cap_are_dropped() {
    let addr = start(1, Duration::from_secs(30)).await;
    let mut first = TcpStream::connect(&addr).await.unwrap();
    first
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut second = TcpStream::connect(&addr).await.unwrap();
    assert!(closed_within(&mut second, Duration::from_secs(2)).await);
    // The first connection is still being held open, not closed.
    assert!(!closed_within(&mut first, Duration::from_millis(300)).await);
}

#[tokio::test]
async fn a_second_connection_from_one_non_loopback_address_is_dropped() {
    let Some(local) = non_loopback_local_ip() else {
        return;
    };
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    start_with_caps(listener, 8, 1).await;

    let mut first = TcpStream::connect((local, port)).await.unwrap();
    first
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut second = TcpStream::connect((local, port)).await.unwrap();
    assert!(
        closed_within(&mut second, Duration::from_secs(2)).await,
        "the per-address cap drops the second connection"
    );
    assert!(!closed_within(&mut first, Duration::from_millis(300)).await);
}

#[tokio::test]
async fn loopback_is_exempt_from_the_per_address_cap() {
    let addr = start_with_caps(TcpListener::bind("127.0.0.1:0").await.unwrap(), 8, 1).await;
    let mut first = TcpStream::connect(&addr).await.unwrap();
    first
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let body = reqwest::get(format!("http://{addr}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "ok", "a second loopback connection is not capped");
    assert!(!closed_within(&mut first, Duration::from_millis(300)).await);
}
