use std::sync::Arc;

use reiki::config::Config;
use reiki::plugins::{PluginChain, builtin};
use reiki::proxy::{App, Secret};

fn golden(name: &str, actual: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("REIKI_BLESS_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn normalize(text: &str) -> String {
    text.replace(env!("CARGO_PKG_VERSION"), "<version>")
}

#[tokio::test]
async fn full_surface_openapi_matches_golden() {
    let root = std::env::temp_dir().join(format!("reiki-openapi-full-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let reqlog = root.join("requests.jsonl");
    let cfg = format!(
        r#"
[admin]
listen = "127.0.0.1:0"
token_sha256 = "10a4c7c9fc5206d6f36dc6944a81bb6f4a3cb0e25014ae3b12e6c3e52712292a"

[upstream.zen]
base_url = "https://example.com/v1"

[keys.k]
upstream = "zen"
file = "/dev/null"

[streams.s]
chain = ["k"]
token_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[peers]
enabled = true
publish = true
listen = "127.0.0.1:0"
peer_token_sha256 = "493a178d1badd6ef2187860f907ec6be297f4154380d4c2a4952936655ea66fb"

[stream_admin]
token_sha256 = "c0002cebadb6aaba3c9310e9bc019575fd4f63bf2daeab32553f9db046872b6b"
actor_id_sha256 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
state_file = "stream-admin.json"
audit_log = "audit.jsonl"
max_streams = 8
max_tokens_per_stream = 4
default_ttl = 3600
max_ttl = 86400
max_body_bytes = 65536
lockout_threshold = 10
lockout_secs = 900
rpm = 60
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

[plugins.requestlog]
enabled = true
file = "{reqlog}"

[plugins.toolpolicy]
enabled = true
"#,
        reqlog = reqlog.display(),
    );
    #[cfg(feature = "plugin-cost")]
    let cfg = {
        let mut cfg = cfg;
        cfg.push_str(
            r#"
[plugins.cost]

[plugins.cost.prices.zen]
currency = "USD"

[plugins.cost.prices.zen.models."*"]
input_per_million = "1"
output_per_million = "1"
"#,
        );
        cfg
    };
    let config = Config::parse(&cfg).unwrap();
    let sa = config.stream_admin.clone().unwrap();
    let keys = [("k".to_owned(), Secret::new("test-key"))].into();
    let plugins = PluginChain::build(&config.plugins, &builtin()).unwrap();
    let app: Arc<App> = App::new(config, keys, reiki::chain::Policy::default(), plugins);
    let manager = reiki::stream_admin::Manager::new(&sa, &root);
    app.set_stream_admin(manager);
    let router = reiki::admin::router(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let res = reqwest::Client::new()
        .get(format!("http://{addr}/v1/openapi.json"))
        .header("authorization", "Bearer admin-token")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let actual = normalize(&res.text().await.unwrap());
    let value: serde_json::Value = serde_json::from_str(&actual).unwrap();
    let pretty = serde_json::to_string_pretty(&value).unwrap() + "\n";
    #[cfg(feature = "plugin-cost")]
    let golden_name = "openapi-full-surface.cost.json";
    #[cfg(not(feature = "plugin-cost"))]
    let golden_name = "openapi-full-surface.json";
    let expected = golden(golden_name, &pretty);
    assert_eq!(pretty, normalize(&expected));
    server.abort();
}
