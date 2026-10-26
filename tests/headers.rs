use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{load_keys, router, App};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Accepts one request and returns its head exactly as it arrived on the wire.
async fn capture() -> (u16, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .await
            .unwrap();
        String::from_utf8_lossy(&head).into_owned()
    });
    (port, task)
}

async fn head_sent_with(label: &str, upstream_extra: &str) -> String {
    let (port, task) = capture().await;
    let dir = std::env::temp_dir().join(format!("reiki-headers-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("key"), "upstream-key").unwrap();
    let text = format!(
        "[upstream.api]\nbase_url = \"http://127.0.0.1:{port}/v1\"\n{upstream_extra}\n[keys.k]\nupstream = \"api\"\nfile = \"{}\"\n[streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"k\"]\n",
        dir.join("key").display(),
        reiki::auth::sha256_hex("tok")
    );
    let config = Config::parse(&text).unwrap();
    let app = App::new(
        Config::parse(&text).unwrap(),
        load_keys(&config).unwrap(),
        Policy::default(),
        PluginChain::default(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/api/chat"))
        .bearer_auth("tok")
        .header("x-custom-thing", "1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    task.await.unwrap()
}

fn header_lines(head: &str) -> Vec<&str> {
    head.lines().skip(1).filter(|l| !l.is_empty()).collect()
}

#[tokio::test]
async fn by_default_names_are_lowercase_with_the_credential_first_and_no_connection_header() {
    let head = head_sent_with("default", "").await;
    let lines = header_lines(&head);
    assert_eq!(lines[0], "authorization: Bearer upstream-key", "{head}");
    assert!(lines.contains(&"x-custom-thing: 1"), "{head}");
    assert!(
        !head.to_ascii_lowercase().contains("\r\nconnection:"),
        "{head}"
    );
}

#[tokio::test]
async fn title_case_and_keep_alive_are_sent_when_asked_for() {
    let head = head_sent_with(
        "styled",
        "title_case_headers = true\nsend_keep_alive = true",
    )
    .await;
    let lines = header_lines(&head);
    assert_eq!(lines[0], "Authorization: Bearer upstream-key", "{head}");
    assert!(lines.contains(&"X-Custom-Thing: 1"), "{head}");
    assert!(lines.contains(&"Connection: keep-alive"), "{head}");
    assert!(
        lines.contains(&"Content-Type: application/json") || !head.contains("content-type"),
        "{head}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.chars().next().is_some_and(|c| c.is_ascii_lowercase())),
        "{head}"
    );
}
