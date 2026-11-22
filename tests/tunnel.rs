use std::io::Write;
use std::sync::{Arc, Mutex};

use reiki::config::TunnelConfig;
use reiki::tunnel::{Tunnel, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(Arc::clone(&self.0))
    }
}

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
        deny_private: false,
        listen: String::new(),
        allow: allow.to_vec(),
        connect_timeout_secs: 5,
        allow_loopback,
        close_idle_on_stop: true,
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
            "[tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = [\"example.com.\"]\n",
            "not a valid allow entry",
        ),
        (
            "[tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = [\" example.com\"]\n",
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
        "{base}[tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = [\"opencode.ai\", \"[::1]:443\"]\n"
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

#[test]
fn the_tunnel_stop_mode_defaults_to_the_safe_behaviour_and_rejects_unknown_keys() {
    use reiki::config::Config;

    let base = "[upstream.u]\nbase_url = \"https://a.example/v1\"\n\
                [tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = [\"a.example\"]\n";
    let enabled = Config::parse(base).unwrap().tunnel.unwrap();
    assert!(enabled.close_idle_on_stop);
    let explicit = Config::parse(&format!("{base}close_idle_on_stop = false\n"))
        .unwrap()
        .tunnel
        .unwrap();
    assert!(!explicit.close_idle_on_stop);
    assert!(Config::parse(&format!("{base}unknown = true\n")).is_err());
}

#[test]
fn changing_the_tunnel_stop_mode_is_a_live_reload_change() {
    use reiki::config::Config;

    let base = "[upstream.u]\nbase_url = \"https://a.example/v1\"\n\
                [tunnel]\nlisten = \"127.0.0.1:4099\"\nallow = [\"a.example\"]\n";
    let running = Config::parse(base).unwrap().tunnel.unwrap();
    let wanted = Config::parse(&format!("{base}close_idle_on_stop = false\n"))
        .unwrap()
        .tunnel
        .unwrap();
    let plan = Tunnel::new(&running).plan_reload(&wanted).unwrap();
    assert!(
        plan.changed
            .iter()
            .any(|line| line.contains("close_idle_on_stop")),
        "{:?}",
        plan.changed
    );
}

#[tokio::test]
async fn stopping_closes_idle_tunnels_at_once_and_cuts_active_ones_after_the_drain() {
    use std::time::Duration;

    use reiki::tunnel::serve_until;

    let port = echo().await;
    let config = TunnelConfig {
        deny_private: false,
        listen: String::new(),
        allow: vec![format!("127.0.0.1:{port}")],
        connect_timeout_secs: 5,
        allow_loopback: true,
        close_idle_on_stop: true,
        intercept: None,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_until(
        listener,
        Arc::new(Tunnel::new(&config)),
        async {
            let _ = rx.await;
        },
        Duration::from_millis(800),
    ));

    let mut idle = TcpStream::connect(addr).await.unwrap();
    let mut active = TcpStream::connect(addr).await.unwrap();
    for client in [&mut idle, &mut active] {
        client
            .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(
            status_line(client).await,
            "HTTP/1.1 200 Connection Established"
        );
    }
    active.write_all(b"alive").await.unwrap();
    let mut buf = [0u8; 5];
    active.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"alive");
    // The idle tunnel has no byte moving in either direction for longer than the idle window.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    active.write_all(b"still").await.unwrap();
    let mut buf = [0u8; 5];
    active.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"still");
    tx.send(()).unwrap();

    let mut one = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_millis(250), idle.read(&mut one))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closed, 0, "idle tunnels close as soon as the stop begins");

    active.write_all(b"later").await.unwrap();
    let mut buf = [0u8; 5];
    tokio::time::timeout(Duration::from_millis(250), active.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        &buf, b"later",
        "active tunnels keep working during the drain"
    );

    let closed = tokio::time::timeout(Duration::from_millis(1_200), active.read(&mut one))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closed, 0, "active tunnels are cut at the drain deadline");
    server.await.unwrap();
}

#[tokio::test]
async fn a_connection_still_sending_its_connect_head_is_cut_with_the_drain() {
    use std::time::Duration;

    use reiki::tunnel::serve_until;

    let config = TunnelConfig {
        deny_private: false,
        listen: String::new(),
        allow: vec!["*".to_owned()],
        connect_timeout_secs: 5,
        allow_loopback: true,
        close_idle_on_stop: true,
        intercept: None,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_until(
        listener,
        Arc::new(Tunnel::new(&config)),
        async {
            let _ = rx.await;
        },
        Duration::from_millis(200),
    ));

    // The handler is still waiting for a request head when the stop begins; it must not keep
    // this socket alive until the ten-second head timeout.
    let mut client = TcpStream::connect(addr).await.unwrap();
    tx.send(()).unwrap();
    let mut one = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_millis(2_000), client.read(&mut one))
        .await
        .expect("the unfinished CONNECT is cut with the drain")
        .unwrap();
    assert_eq!(closed, 0, "a half-open head is closed at the cut");
    server.await.unwrap();
}

#[test]
fn a_shutdown_cut_logs_the_bytes_already_moved() {
    use std::time::Duration;

    use reiki::tunnel::serve_until;

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let port = echo().await;
            let config = TunnelConfig {
                deny_private: false,
                listen: String::new(),
                allow: vec![format!("127.0.0.1:{port}")],
                connect_timeout_secs: 5,
                allow_loopback: true,
                close_idle_on_stop: true,
                intercept: None,
            };
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(serve_until(
                listener,
                Arc::new(Tunnel::new(&config)),
                async {
                    let _ = rx.await;
                },
                Duration::from_millis(300),
            ));

            let mut client = TcpStream::connect(addr).await.unwrap();
            client
                .write_all(
                    format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            assert_eq!(
                status_line(&mut client).await,
                "HTTP/1.1 200 Connection Established"
            );
            client.write_all(b"hello").await.unwrap();
            let mut echoed = [0u8; 5];
            client.read_exact(&mut echoed).await.unwrap();
            assert_eq!(&echoed, b"hello");

            tx.send(()).unwrap();
            let mut one = [0u8; 1];
            let closed = tokio::time::timeout(Duration::from_millis(2_000), client.read(&mut one))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(closed, 0, "the cut closes the tunnel");
            server.await.unwrap();
        });
    });

    let text = logs.text();
    let line = text
        .lines()
        .find(|line| line.contains("tunnel closed"))
        .unwrap_or_else(|| panic!("no tunnel close line was logged: {text}"));
    assert!(line.contains("bytes_up=5"), "lost upload total: {line}");
    assert!(line.contains("bytes_down=5"), "lost download total: {line}");
}
