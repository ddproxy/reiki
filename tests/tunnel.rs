use std::sync::Arc;

use reiki::config::TunnelConfig;
use reiki::tunnel::{serve, Tunnel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Echoes every byte back, standing in for a TLS server the tunnel must not read.
async fn echo() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

async fn tunnel(allow: &[String], allow_loopback: bool) -> String {
    let config = TunnelConfig {
        listen: String::new(),
        allow: allow.to_vec(),
        connect_timeout_secs: 5,
        allow_loopback,
        intercept: None,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(serve(listener, Arc::new(Tunnel::new(&config))));
    addr
}

async fn status_line(client: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if client.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn an_allowed_destination_is_reached_and_bytes_pass_untouched() {
    let port = echo().await;
    let addr = tunnel(&[format!("127.0.0.1:{port}")], true).await;
    let mut client = TcpStream::connect(&addr).await.unwrap();

    // The first payload bytes ride in the same write as the request head.
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: x\r\n\r\n\x16\x03\x01hello");
    client.write_all(request.as_bytes()).await.unwrap();

    assert_eq!(
        status_line(&mut client).await,
        "HTTP/1.1 200 Connection Established"
    );
    let mut early = [0u8; 8];
    client.read_exact(&mut early).await.unwrap();
    assert_eq!(&early, b"\x16\x03\x01hello");
    client.write_all(&[0, 255, 7, 13, 10]).await.unwrap();
    let mut echoed = [0u8; 5];
    client.read_exact(&mut echoed).await.unwrap();
    assert_eq!(echoed, [0, 255, 7, 13, 10]);
}

#[tokio::test]
async fn a_destination_that_is_not_allowed_is_refused() {
    let port = echo().await;
    let addr = tunnel(&["example.com".to_owned()], true).await;
    let mut client = TcpStream::connect(&addr).await.unwrap();

    client
        .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .unwrap();

    assert_eq!(status_line(&mut client).await, "HTTP/1.1 403 Forbidden");
}

#[tokio::test]
async fn local_addresses_are_refused_unless_explicitly_allowed() {
    let port = echo().await;
    let addr = tunnel(&["*".to_owned(), format!("127.0.0.1:{port}")], false).await;
    let mut client = TcpStream::connect(&addr).await.unwrap();

    client
        .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .unwrap();

    assert_eq!(status_line(&mut client).await, "HTTP/1.1 502 Bad Gateway");
}

#[tokio::test]
async fn anything_but_connect_is_a_bad_request() {
    let addr = tunnel(&["*".to_owned()], true).await;
    let mut client = TcpStream::connect(&addr).await.unwrap();

    client
        .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .unwrap();

    assert_eq!(status_line(&mut client).await, "HTTP/1.1 400 Bad Request");
}

#[test]
fn the_tunnel_section_is_validated() {
    use reiki::config::Config;
    let base = "[upstream.u]\nbase_url = \"https://a.example/v1\"\n";
    for (section, wanted) in [
        (
            "[tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = []\n",
            "no destinations",
        ),
        (
            "[tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = [\"a*b\"]\n",
            "not a valid allow entry",
        ),
        (
            "[tunnel]\nlisten = \"127.0.0.1:4097\"\nallow = [\"*\"]\n",
            "must differ",
        ),
    ] {
        let error = Config::parse(&format!("{base}{section}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(wanted), "{error}");
    }
    assert!(Config::parse(&format!(
        "{base}[tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = [\"opencode.ai\"]\n"
    ))
    .is_ok());
}

#[tokio::test]
async fn one_peer_cannot_hold_more_than_its_share_of_tunnels() {
    let addr = tunnel(&["127.0.0.1:1".to_owned()], true).await;
    // Idle connections that never send a request still occupy a slot.
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(TcpStream::connect(&addr).await.unwrap());
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut extra = TcpStream::connect(&addr).await.unwrap();
    assert!(status_line(&mut extra).await.contains("503"));
    drop(held);
}
