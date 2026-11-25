//! `GET /v1/health` on the peer listener: its own socket, its own token, nothing else served.
#![cfg(feature = "peers")]

mod common;
use common::write_key;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{App, Secret, router};

const ADMIN_TOKEN: &str = "admin-token-for-peer-tests";
const STREAM_TOKEN: &str = "stream-token-for-peer-tests";
const PEER_TOKEN: &str = "peer-token-for-peer-tests";

fn sha(token: &str) -> String {
    reiki::auth::sha256_hex(token)
}

/// Serves `router` through Reiki's own loop, which records the peer address the throttle needs.
async fn serve_on(listener: tokio::net::TcpListener, router: Router) -> String {
    let addr = listener.local_addr().unwrap();
    tokio::spawn(reiki::serve::serve(
        listener,
        router,
        64,
        std::future::pending(),
    ));
    format!("http://{addr}")
}

async fn bind() -> (tokio::net::TcpListener, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// An upstream that refuses every key with a rate limit.
async fn limited_upstream() -> String {
    let (listener, addr) = bind().await;
    let app = Router::new().fallback(|| async {
        let mut response = (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response();
        response
            .headers_mut()
            .insert("retry-after", "30".parse().unwrap());
        response
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn config_text(upstream: &str, peer_addr: SocketAddr, peers_extra: &str) -> String {
    format!(
        "[upstream.zen]\nbase_url = \"{upstream}/v1\"\n\
         [keys.zen-a]\nupstream = \"zen\"\nfile = \"/unused/zen-a\"\n\
         [keys.zen-b]\nupstream = \"zen\"\nfile = \"/unused/zen-b\"\n\
         [streams.client-a]\ntoken_sha256 = \"{}\"\nchain = [\"zen-a\", \"zen-b\"]\n\
         [admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n\
         [plugins.metrics]\n\
         [peers]\nenabled = true\npublish = true\nlisten = \"{peer_addr}\"\n\
         peer_token_sha256 = \"{}\"\ninstance = \"peer-test\"\n{peers_extra}",
        sha(STREAM_TOKEN),
        sha(ADMIN_TOKEN),
        sha(PEER_TOKEN),
    )
}

struct Rig {
    app: Arc<App>,
    relay: String,
    admin: String,
    peer: String,
    peer_addr: SocketAddr,
    upstream: String,
    client: reqwest::Client,
}

/// An upstream that answers every request with a success and an OpenAI-style usage object.
async fn usage_upstream() -> String {
    let (listener, addr) = bind().await;
    let app = Router::new().fallback(|| async {
        let body = serde_json::json!({
            "id": "chatcmpl-test",
            "usage": {"prompt_tokens": 6, "completion_tokens": 4, "total_tokens": 10}
        });
        (StatusCode::OK, body.to_string())
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

impl Rig {
    async fn new(peers_extra: &str) -> Self {
        Self::with_upstream(peers_extra, limited_upstream().await).await
    }

    async fn with_upstream(peers_extra: &str, upstream: String) -> Self {
        let (peer_listener, peer_addr) = bind().await;
        let config = Config::parse(&config_text(&upstream, peer_addr, peers_extra)).unwrap();
        let plugins =
            reiki::plugins::PluginChain::build(&config.plugins, &reiki::plugins::builtin())
                .unwrap();
        let keys: HashMap<String, Secret> = ["zen-a", "zen-b"]
            .iter()
            .map(|name| ((*name).to_owned(), Secret::new(format!("upstream-{name}"))))
            .collect();
        let app = App::with_plugins(
            config,
            keys,
            Policy::default(),
            Duration::from_secs(120),
            plugins,
        );
        let relay = serve_on(bind().await.0, router(Arc::clone(&app))).await;
        let admin = serve_on(bind().await.0, reiki::admin::router(Arc::clone(&app))).await;
        let peer = serve_on(peer_listener, reiki::peers::router(Arc::clone(&app))).await;
        Self {
            app,
            relay,
            admin,
            peer,
            peer_addr,
            upstream,
            client: reqwest::Client::new(),
        }
    }

    async fn peer_get(&self, path: &str, token: Option<&str>) -> (u16, String) {
        let mut request = self.client.get(format!("{}{path}", self.peer));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let res = request.send().await.unwrap();
        (res.status().as_u16(), res.text().await.unwrap())
    }

    async fn admin_metrics(&self) -> String {
        self.client
            .get(format!("{}/v1/metrics", self.admin))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    async fn health(&self) -> serde_json::Value {
        let (code, body) = self.peer_get("/v1/health", Some(PEER_TOKEN)).await;
        assert_eq!(
            code, 200,
            "the peer listener must answer the health request"
        );
        serde_json::from_str(&body).unwrap()
    }
}

#[tokio::test]
async fn only_the_peer_token_opens_the_peer_listener() {
    let rig = Rig::new("").await;
    for (i, token) in [
        None,
        Some(ADMIN_TOKEN),
        Some(STREAM_TOKEN),
        Some("not-a-token"),
    ]
    .into_iter()
    .enumerate()
    {
        let (code, body) = rig.peer_get("/v1/health", token).await;
        assert_eq!(code, 401, "token #{i} is refused");
        let err: serde_json::Value = serde_json::from_str(&body).expect("refusal is json");
        assert_eq!(err["ok"], false, "token #{i} refusal is envelope");
        assert!(err["error"]["code"].is_string(), "token #{i} has code");
    }
    let other_carriers = [
        rig.client
            .get(format!("{}/v1/health", rig.peer))
            .header("x-api-key", PEER_TOKEN),
        rig.client
            .get(format!("{}/v1/health?key={PEER_TOKEN}", rig.peer)),
        rig.client
            .get(format!("{}/v1/health", rig.peer))
            .header("authorization", format!("Basic {PEER_TOKEN}")),
    ];
    for request in other_carriers {
        assert_eq!(request.send().await.unwrap().status(), 401);
    }
    let res = rig
        .client
        .get(format!("{}/v1/health", rig.peer))
        .bearer_auth(PEER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["content-type"], "application/json");
}

#[tokio::test]
async fn the_peer_token_works_nowhere_else() {
    let rig = Rig::new("").await;
    for path in [
        "/status",
        "/v1/status",
        "/v1/openapi.json",
        "/v1/metrics",
        "/v1/usage/streams",
    ] {
        let res = rig
            .client
            .get(format!("{}{path}", rig.admin))
            .bearer_auth(PEER_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "admin {path}");
    }
    for path in ["/v1/reload?dry_run=true", "/v1/keys/enable?key=zen-a"] {
        let res = rig
            .client
            .post(format!("{}{path}", rig.admin))
            .bearer_auth(PEER_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "admin {path}");
    }
    let res = rig
        .client
        .post(format!("{}/zen/chat/completions", rig.relay))
        .bearer_auth(PEER_TOKEN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401, "client listener");
    let res = rig
        .client
        .get(format!("{}/v1/health", rig.admin))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        404,
        "the admin listener does not serve health"
    );
}

#[tokio::test]
async fn every_other_path_and_method_on_the_peer_listener_is_404() {
    let rig = Rig::new("").await;
    for path in [
        "/",
        "/health",
        "/v1/health/",
        "/v1/healthz",
        "/v1/health/x",
        "/status",
        "/v1/status",
        "/v1/openapi.json",
        "/v1/metrics",
        "/v1/usage/streams",
        "/reload",
        "/__reiki/healthz",
        "/zen/chat/completions",
    ] {
        for (i, token) in [None, Some(PEER_TOKEN), Some(ADMIN_TOKEN)]
            .into_iter()
            .enumerate()
        {
            let (code, _) = rig.peer_get(path, token).await;
            assert_eq!(code, 404, "unexpected path with token #{i}");
        }
    }
    let res = rig
        .client
        .post(format!("{}/v1/health", rig.peer))
        .bearer_auth(PEER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404, "POST");
}

#[tokio::test]
async fn health_counts_keys_per_upstream_without_names_paths_or_secrets() {
    // Exact cooldowns here; the default bucketing has its own test.
    let rig = Rig::new("cooldown_bucket_secs = 0\n").await;
    let doc = rig.health().await;
    assert_eq!(doc["instance"], "peer-test");
    assert_eq!(doc["reiki_version"], env!("CARGO_PKG_VERSION"));
    let observed = doc["observed_at"].as_str().unwrap();
    assert!(
        observed.len() == 20 && observed.ends_with('Z'),
        "{observed}"
    );
    let zen = &doc["upstreams"][0];
    assert_eq!(zen["name"], "zen");
    assert_eq!(zen["healthy_keys"], 2);
    assert_eq!(zen["cooling_keys"], 0);
    assert!(zen["last_status_class"].is_null());
    assert!(zen.get("keys").is_none());

    // Both keys answer 429, so the chain cools them one after the other.
    let res = rig
        .client
        .post(format!("{}/zen/chat/completions", rig.relay))
        .bearer_auth(STREAM_TOKEN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    let doc = rig.health().await;
    let zen = &doc["upstreams"][0];
    assert_eq!(zen["healthy_keys"], 0);
    assert_eq!(zen["cooling_keys"], 2);
    assert_eq!(zen["disabled_keys"], 0);
    let wait = zen["soonest_cooldown_seconds"].as_u64().unwrap();
    assert!((29..=30).contains(&wait), "{wait}");
    assert_eq!(zen["last_status_class"], "4xx");

    let text = doc.to_string();
    for (i, secret) in [
        "client-a",
        "zen-a",
        "/unused",
        "upstream-zen-a",
        &sha(PEER_TOKEN),
        &rig.upstream,
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            !text.contains(secret),
            "secret #{i} leaked into the health document"
        );
    }
}

#[tokio::test]
async fn usage_counts_are_published_by_default_and_can_be_hidden() {
    let rig = Rig::new("cooldown_bucket_secs = 0\n").await;
    let zen = &rig.health().await["upstreams"][0];
    assert_eq!(zen["requests_1h"], 0);
    assert_eq!(zen["limited_1h"], 0);
    assert_eq!(
        zen["request_buckets"],
        serde_json::json!([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    );

    // Both keys answer 429, so both attempts are counted and limited.
    let res = rig
        .client
        .post(format!("{}/zen/chat/completions", rig.relay))
        .bearer_auth(STREAM_TOKEN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
    let zen = &rig.health().await["upstreams"][0];
    assert_eq!(zen["requests_1h"], 2);
    assert_eq!(zen["limited_1h"], 2);
    let buckets = zen["request_buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 12);
    assert_eq!(buckets[11], 2, "the newest bucket holds the two attempts");
    let minutes = zen["request_minutes"].as_array().unwrap();
    assert_eq!(minutes.len(), 10);
    assert_eq!(minutes[9], 2, "the newest minute holds the two attempts");

    let hidden = Rig::new("expose_usage = false\n").await;
    let zen = &hidden.health().await["upstreams"][0];
    for field in [
        "requests_1h",
        "limited_1h",
        "request_buckets",
        "request_minutes",
        "token_minutes",
    ] {
        assert!(zen.get(field).is_none(), "{field} must be hidden");
    }
}

#[tokio::test]
async fn tokens_are_counted_from_each_response_per_upstream() {
    let rig = Rig::with_upstream("cooldown_bucket_secs = 0\n", usage_upstream().await).await;
    assert_eq!(rig.health().await["upstreams"][0]["tokens_1h"], 0);
    for _ in 0..2 {
        assert_eq!(rig.send(None).await, 200);
    }
    let zen = &rig.health().await["upstreams"][0];
    assert_eq!(zen["requests_1h"], 2);
    assert_eq!(zen["limited_1h"], 0);
    assert_eq!(zen["tokens_1h"], 20, "prompt plus completion, per response");
    let buckets = zen["token_buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 12);
    assert_eq!(buckets[11], 20);
    let minutes = zen["token_minutes"].as_array().unwrap();
    assert_eq!(minutes.len(), 10);
    assert_eq!(minutes[9], 20);

    let hidden = Rig::with_upstream("expose_usage = false\n", usage_upstream().await).await;
    assert_eq!(hidden.send(None).await, 200);
    let zen = &hidden.health().await["upstreams"][0];
    for field in ["tokens_1h", "token_buckets"] {
        assert!(zen.get(field).is_none(), "{field} must be hidden");
    }
}

#[tokio::test]
async fn metrics_gauges_equal_the_usage_the_health_document_reports() {
    let successes = Rig::with_upstream("cooldown_bucket_secs = 0\n", usage_upstream().await).await;
    for _ in 0..2 {
        assert_eq!(successes.send(None).await, 200);
    }
    let limited = Rig::new("cooldown_bucket_secs = 0\n").await;
    assert_eq!(limited.send(None).await, 503);

    for rig in [&successes, &limited] {
        let zen = rig.health().await["upstreams"][0].clone();
        let text = rig.admin_metrics().await;
        for (family, field) in [
            ("reiki_upstream_requests_1h", "requests_1h"),
            ("reiki_upstream_limited_1h", "limited_1h"),
            ("reiki_upstream_tokens_1h", "tokens_1h"),
        ] {
            let sample = format!("{family}{{upstream=\"zen\"}} {}", zen[field]);
            assert!(
                text.lines().any(|line| line == sample),
                "`{sample}` is in the scrape: {text}"
            );
        }
    }

    let hidden = Rig::with_upstream("expose_usage = false\n", usage_upstream().await).await;
    assert_eq!(hidden.send(None).await, 200);
    assert!(!hidden.admin_metrics().await.contains("reiki_upstream_"));
}

#[tokio::test]
async fn key_labels_appear_only_when_exposed() {
    let rig = Rig::new("expose_key_labels = true\n").await;
    let doc = rig.health().await;
    let keys = doc["upstreams"][0]["keys"].as_array().unwrap();
    let names: Vec<&str> = keys.iter().map(|k| k["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["zen-a", "zen-b"]);
    assert!(keys.iter().all(|k| k["state"] == "available"));
    assert!(!doc.to_string().contains("client-a"), "streams stay hidden");
}

#[tokio::test]
async fn a_refused_peer_login_asks_to_close_the_connection() {
    let rig = Rig::new("").await;
    let res = rig
        .client
        .get(format!("{}/v1/health", rig.peer))
        .bearer_auth("stale")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(
        res.headers()
            .get(reqwest::header::CONNECTION)
            .and_then(|value| value.to_str().ok()),
        Some("close")
    );
}

#[tokio::test]
async fn loopback_failures_lock_out_the_address_but_not_the_admin_listener() {
    // Every tunnelled peer arrives from loopback, so a stale token locks out the other peers
    // behind that address too. The peer listener's counts stay separate from the admin's.
    let rig = Rig::new("").await;
    for _ in 0..9 {
        assert_eq!(rig.peer_get("/v1/health", Some("stale")).await.0, 401);
    }
    let res = rig
        .client
        .get(format!("{}/v1/health", rig.peer))
        .bearer_auth(PEER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        200,
        "below the threshold a valid token gets in"
    );
    assert_eq!(rig.peer_get("/v1/health", Some("stale")).await.0, 401);
    let res = rig
        .client
        .get(format!("{}/v1/health", rig.peer))
        .bearer_auth(PEER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429, "the address is locked out");
    assert!(res.headers().contains_key(reqwest::header::RETRY_AFTER));
    assert_eq!(
        res.headers()
            .get(reqwest::header::CONNECTION)
            .and_then(|value| value.to_str().ok()),
        Some("close")
    );
    let res = rig
        .client
        .get(format!("{}/v1/status", rig.admin))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "peer failures do not lock out the admin");
}

#[tokio::test]
async fn concurrent_stale_logins_still_lock_out_the_shared_address() {
    let second = "second-peer-token-behind-the-tunnel";
    let rig = Rig::new(&format!(
        "[[peers.tokens]]\nname = \"peer-b\"\ntoken_sha256 = \"{}\"\n",
        sha(second)
    ))
    .await;
    let stale = (0..12).map(|_| rig.peer_get("/v1/health", Some("revoked-token")));
    for (code, _) in futures_util::future::join_all(stale).await {
        assert!(matches!(code, 401 | 429), "{code}");
    }
    for (i, token) in [PEER_TOKEN, second].into_iter().enumerate() {
        assert_eq!(
            rig.peer_get("/v1/health", Some(token)).await.0,
            429,
            "peer token #{i} is refused"
        );
    }
}

#[tokio::test]
async fn a_concurrent_burst_cannot_all_pass_the_login_check() {
    // With a one-failure threshold only the very first concurrent guess may be authenticated
    // and counted; the rest must see the reservation made under the same lock.
    let rig = Rig::new("[auth]\nmax_failures = 1\n").await;
    let burst = futures_util::future::join_all(
        (0..16).map(|_| rig.peer_get("/v1/health", Some("revoked-token"))),
    )
    .await;
    let allowed = burst.iter().filter(|(code, _)| *code == 401).count();
    assert_eq!(
        allowed,
        1,
        "statuses: {:?}",
        burst.iter().map(|(c, _)| c).collect::<Vec<_>>()
    );
    assert_eq!(
        burst.iter().filter(|(code, _)| *code == 429).count(),
        15,
        "every other guess is refused"
    );
    assert_eq!(rig.peer_get("/v1/health", Some(PEER_TOKEN)).await.0, 429);
}

#[tokio::test]
async fn the_openapi_document_describes_health_on_the_peer_listener() {
    let rig = Rig::new("").await;
    let text = rig
        .client
        .get(format!("{}/v1/openapi.json", rig.admin))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(doc["components"]["securitySchemes"]["peerAuth"].is_object());
    let paths = doc["paths"].as_object().unwrap();
    for (path, item) in paths {
        let server = item["servers"][0]["url"].as_str();
        let (base, token) = match server {
            Some(url) => (url.to_owned(), PEER_TOKEN),
            None => (rig.admin.clone(), ADMIN_TOKEN),
        };
        let method = item
            .as_object()
            .unwrap()
            .keys()
            .find(|k| *k != "servers")
            .unwrap()
            .clone();
        let request = if method == "post" {
            rig.client.post(format!("{base}{path}?dry_run=true"))
        } else {
            rig.client.get(format!("{base}{path}"))
        };
        let status = request.bearer_auth(token).send().await.unwrap().status();
        assert_ne!(status, 404, "documented but not served: {base}{path}");
    }
    let health = &paths["/v1/health"];
    assert_eq!(
        health["servers"][0]["url"],
        format!("http://{}", rig.peer_addr)
    );
    assert_eq!(
        health["get"]["security"][0]["peerAuth"],
        serde_json::json!([])
    );
    let top = &health["get"]["responses"]["200"]["content"]["application/json"]["schema"];
    assert_eq!(top["properties"]["reiki_version"]["type"], "string");
    let required = top["required"].as_array().unwrap();
    for field in ["reiki_version", "age_seconds", "failures"] {
        assert!(
            required.iter().any(|name| name == field),
            "{field} is required in the current wire shape"
        );
    }
    let upstream = &health["get"]["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
        ["upstreams"]["items"];
    for field in [
        "requests_1h",
        "limited_1h",
        "request_buckets",
        "request_minutes",
        "token_minutes",
    ] {
        assert!(
            upstream["properties"].get(field).is_some(),
            "{field} is described"
        );
        assert!(
            !upstream["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == field),
            "{field} stays optional"
        );
    }
    let key = &upstream["properties"]["keys"]["items"];
    assert_eq!(key["properties"]["quota_reset_at"]["format"], "date-time");
}

#[tokio::test]
async fn turning_publishing_off_on_reload_closes_the_route_and_asks_for_a_restart() {
    let rig = Rig::new("").await;
    let dir = std::env::temp_dir().join(format!("reiki-peers-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["zen-a", "zen-b"] {
        write_key(dir.join(name), format!("upstream-{name}\n"));
    }
    let text = config_text(&rig.upstream, rig.peer_addr, "")
        .replace("/unused/", &format!("{}/", dir.display()))
        .replace("publish = true", "publish = false");
    let path = dir.join("reiki.toml");
    std::fs::write(&path, text).unwrap();
    let report = rig.app.reload_from_file(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        report.restart_needed.iter().any(|r| r == "peers.listen"),
        "{:?}",
        report.restart_needed
    );
    let (code, _) = rig.peer_get("/v1/health", Some(PEER_TOKEN)).await;
    assert_eq!(code, 404, "the route closes at once");
}

#[tokio::test]
async fn a_changed_server_or_service_table_on_reload_asks_for_a_restart() {
    let rig = Rig::new("").await;
    let dir = std::env::temp_dir().join(format!("reiki-server-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["zen-a", "zen-b"] {
        write_key(dir.join(name), format!("upstream-{name}\n"));
    }
    let text = config_text(&rig.upstream, rig.peer_addr, "")
        .replace("/unused/", &format!("{}/", dir.display()));
    let text =
        format!("{text}\n[server]\nmax_connections = 500\n[service]\nshutdown_timeout_secs = 40\n");
    let path = dir.join("reiki.toml");
    std::fs::write(&path, text).unwrap();
    let report = rig.app.reload_from_file(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    for wanted in ["server.max_connections", "service.shutdown_timeout_secs"] {
        assert!(
            report.restart_needed.iter().any(|r| r == wanted),
            "{wanted} missing from {:?}",
            report.restart_needed
        );
    }
}

#[tokio::test]
async fn a_peer_token_hash_shared_with_another_credential_is_refused() {
    let upstream = "http://127.0.0.1:9";
    let addr: SocketAddr = "127.0.0.1:4999".parse().unwrap();
    for reused in [ADMIN_TOKEN, STREAM_TOKEN] {
        let text = config_text(upstream, addr, "").replace(&sha(PEER_TOKEN), &sha(reused));
        let error = Config::parse(&text).unwrap_err().to_string();
        assert!(error.contains("peer_token_sha256 is the same"), "{error}");
    }
}

async fn cool_both(rig: &Rig) {
    let res = rig
        .client
        .post(format!("{}/zen/chat/completions", rig.relay))
        .bearer_auth(STREAM_TOKEN)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 503);
}

#[tokio::test]
async fn cooldowns_are_rounded_up_to_the_bucket_by_default() {
    let rig = Rig::new("expose_key_labels = true\n").await;
    cool_both(&rig).await;
    let doc = rig.health().await;
    let zen = &doc["upstreams"][0];
    assert_eq!(
        zen["soonest_cooldown_seconds"], 300,
        "the cooldown must be rounded up"
    );
    for key in zen["keys"].as_array().unwrap() {
        assert_eq!(
            key["cooldown_seconds"], 300,
            "the key cooldown must be bucketed"
        );
    }
    let rig = Rig::new("expose_key_labels = true\ncooldown_bucket_secs = 20\n").await;
    cool_both(&rig).await;
    let doc = rig.health().await;
    assert_eq!(doc["upstreams"][0]["soonest_cooldown_seconds"], 40);
}

#[tokio::test]
async fn any_of_several_named_peer_tokens_opens_health() {
    let second = "second-peer-token-for-peer-tests";
    let rig = Rig::new(&format!(
        "[[peers.tokens]]\nname = \"peer-b\"\ntoken_sha256 = \"{}\"\n",
        sha(second)
    ))
    .await;
    for (i, token) in [PEER_TOKEN, second].into_iter().enumerate() {
        let (code, body) = rig.peer_get("/v1/health", Some(token)).await;
        assert_eq!(code, 200, "peer token #{i} is accepted");
        assert!(
            !body.contains("peer-b"),
            "token names are never published: {body}"
        );
    }
    for (i, token) in [ADMIN_TOKEN, STREAM_TOKEN, "not-a-peer-token"]
        .into_iter()
        .enumerate()
    {
        let (code, _) = rig.peer_get("/v1/health", Some(token)).await;
        assert_eq!(code, 401, "non-peer token #{i} is refused");
    }
}

#[tokio::test]
async fn a_second_peer_token_equal_to_another_credential_is_refused() {
    let addr: SocketAddr = "127.0.0.1:4999".parse().unwrap();
    for (reused, needle) in [
        (
            ADMIN_TOKEN,
            "peer token `peer-b` is the same as the admin token",
        ),
        (
            STREAM_TOKEN,
            "peer token `peer-b` is the same as the token of stream",
        ),
        (
            PEER_TOKEN,
            "peer token `peer-b` is the same as peer token `default`",
        ),
    ] {
        let text = config_text(
            "http://127.0.0.1:9",
            addr,
            &format!(
                "[[peers.tokens]]\nname = \"peer-b\"\ntoken_sha256 = \"{}\"\n",
                sha(reused)
            ),
        );
        let error = Config::parse(&text).unwrap_err().to_string();
        assert!(error.contains(needle), "{error}");
    }
}

impl Rig {
    async fn send(&self, session: Option<&str>) -> u16 {
        let mut request = self
            .client
            .post(format!("{}/zen/chat/completions", self.relay))
            .bearer_auth(STREAM_TOKEN)
            .body("{}");
        if let Some(id) = session {
            request = request.header("x-opencode-session", id);
        }
        request.send().await.unwrap().status().as_u16()
    }
}

#[tokio::test]
async fn host_stats_and_sessions_stay_out_unless_exposed() {
    let rig = Rig::new("").await;
    rig.send(Some("ses_one")).await;
    let doc = rig.health().await;
    assert!(
        doc.get("host").is_none() && doc.get("sessions").is_none(),
        "host and session stats stay hidden"
    );
}

#[tokio::test]
async fn host_stats_report_load_memory_and_disks_by_role() {
    let rig = Rig::new("expose_host_stats = true\n").await;
    let dir = std::env::temp_dir().join(format!("reiki-peers-host-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    rig.app.set_config_path(dir.join("reiki.toml"));
    rig.app.set_state_path(dir.join("reiki.toml.state"));
    let doc = rig.health().await;
    let _ = std::fs::remove_dir_all(&dir);
    let host = &doc["host"];
    assert!(host["cores"].as_u64().unwrap() >= 1);
    if cfg!(target_os = "linux") {
        assert_eq!(host["load_average"].as_array().unwrap().len(), 3);
        let memory = &host["memory"];
        assert!(memory["total_bytes"].as_u64().unwrap() > 0);
        assert!(memory["available_bytes"].is_u64() && memory["swap_total_bytes"].is_u64());
    }
    let disks = host["disks"].as_array().unwrap();
    assert_eq!(disks.len(), 1, "{disks:?}");
    assert_eq!(disks[0]["roles"], serde_json::json!(["config", "state"]));
    assert!(disks[0]["used_percent"].as_f64().unwrap() <= 100.0);
    assert!(
        !doc.to_string().contains(&*dir.to_string_lossy()),
        "no paths are published"
    );
}

#[tokio::test]
async fn sessions_count_distinct_ids_once_per_request_whatever_the_retries() {
    let rig = Rig::new("expose_host_stats = true\n").await;
    // Each request fails over both keys before answering, which must not add sessions.
    for _ in 0..3 {
        rig.send(Some("ses_alpha")).await;
    }
    rig.send(Some("ses_beta")).await;
    rig.send(None).await;
    let doc = rig.health().await;
    let sessions = &doc["sessions"];
    assert_eq!(
        sessions["active_sessions"], 2,
        "the active session count must be two"
    );
    assert_eq!(sessions["window_seconds"], 300);
    assert!(
        sessions.get("streams").is_none(),
        "stream names stay hidden"
    );
    let text = doc.to_string();
    assert!(
        !text.contains("ses_alpha") && !text.contains("client-a"),
        "session ids and stream names stay hidden"
    );
}

#[tokio::test]
async fn per_stream_sessions_need_key_labels_too() {
    let rig = Rig::new("expose_host_stats = true\nexpose_key_labels = true\n").await;
    rig.send(Some("ses_alpha")).await;
    let doc = rig.health().await;
    assert_eq!(
        doc["sessions"]["streams"],
        serde_json::json!([{"stream": "client-a", "active_sessions": 1}])
    );
}
