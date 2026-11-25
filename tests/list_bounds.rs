use std::sync::Arc;

use reiki::config::Config;
use reiki::plugins::{PluginChain, builtin};
use reiki::proxy::{App, Secret};

async fn spawn_admin(app: Arc<App>) -> (String, tokio::task::JoinHandle<()>) {
    let router = reiki::admin::router(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), server)
}

fn base_cfg(extra_streams: usize) -> String {
    let mut out = String::from(
        r#"
[admin]
listen = "127.0.0.1:0"
token_sha256 = "10a4c7c9fc5206d6f36dc6944a81bb6f4a3cb0e25014ae3b12e6c3e52712292a"

[upstream.zen]
base_url = "https://example.com/v1"

[keys.k]
upstream = "zen"
file = "/dev/null"
"#,
    );
    for i in 0..extra_streams {
        out.push_str(&format!(
            "\n[streams.s{i}]\nchain=[\"k\"]\ntoken_sha256=\"{i:064x}\"\n"
        ));
    }
    out
}

#[tokio::test]
async fn status_and_stream_console_cap_lists() {
    let cfg = base_cfg(260);
    let config = Config::parse(&cfg).unwrap();
    let keys = [("k".to_owned(), Secret::new("test-key"))].into();
    let plugins = PluginChain::build(&config.plugins, &builtin()).unwrap();
    let app: Arc<App> = App::new(config, keys, reiki::chain::Policy::default(), plugins);
    let (base, server) = spawn_admin(app).await;
    let client = reqwest::Client::new();

    let status: serde_json::Value = serde_json::from_str(
        &client
            .get(format!("{base}/v1/status"))
            .header("authorization", "Bearer admin-token")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    let streams = status["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 256);
    assert_eq!(status["streams_truncated"], true);

    let console: serde_json::Value = serde_json::from_str(
        &client
            .get(format!("{base}/v1/stream-console"))
            .header("authorization", "Bearer admin-token")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(console["streams"].as_array().unwrap().len(), 256);
    assert_eq!(console["streams_truncated"], true);
    assert_eq!(console["streams_total"], 260);
    server.abort();
}

#[tokio::test]
async fn stream_admin_list_caps_results() {
    let root = std::env::temp_dir().join(format!("reiki-list-bounds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let cfg = String::from(
        r#"
[admin]
listen = "127.0.0.1:0"
token_sha256 = "10a4c7c9fc5206d6f36dc6944a81bb6f4a3cb0e25014ae3b12e6c3e52712292a"

[upstream.zen]
base_url = "https://example.com/v1"

[keys.k]
upstream = "zen"
file = "/dev/null"

[stream_admin]
token_sha256 = "c0002cebadb6aaba3c9310e9bc019575fd4f63bf2daeab32553f9db046872b6b"
actor_id_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
state_file = "stream-admin.json"
audit_log = "audit.jsonl"
max_streams = 300
max_tokens_per_stream = 4
default_ttl = 3600
max_ttl = 86400
max_body_bytes = 65536
lockout_threshold = 1000
lockout_secs = 900
rpm = 1000
allowed_listen = ["127.0.0.1:0"]

[stream_admin.allowed_keys.k]
assignable = true

[stream_admin.ceilings]
rpm = 100
tpm = 100000
concurrent = 10
daily_requests = 1000
daily_tokens = 100000
monthly_requests = 10000
monthly_tokens = 1000000
"#,
    );
    let config = Config::parse(&cfg).unwrap();
    let sa = config.stream_admin.clone().unwrap();
    let keys = [("k".to_owned(), Secret::new("test-key"))].into();
    let plugins = PluginChain::build(&config.plugins, &builtin()).unwrap();
    let app: Arc<App> = App::new(config, keys, reiki::chain::Policy::default(), plugins);
    let manager = reiki::stream_admin::Manager::new(&sa, &root);
    app.set_stream_admin(manager);
    let (base, server) = spawn_admin(app).await;
    let client = reqwest::Client::new();
    for i in 0..260 {
        let res = client
            .post(format!("{base}/v1/streams/create"))
            .header("authorization", "Bearer stream-admin-token")
            .header("idempotency-key", format!("k{i}"))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&serde_json::json!({"name": format!("s{i}"), "key":"k", "ttl":3600, "reason":"test"})).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            200,
            "create {i} failed: {}",
            res.text().await.unwrap()
        );
    }
    let list: serde_json::Value = serde_json::from_str(
        &client
            .get(format!("{base}/v1/streams"))
            .header("authorization", "Bearer stream-admin-token")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list["streams"].as_array().unwrap().len(), 256);
    assert_eq!(list["streams_truncated"], true);
    assert_eq!(list["streams_total"], 260);
    server.abort();
}
