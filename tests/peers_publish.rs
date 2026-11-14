//! `GET /v1/health` on the peer listener: its own socket, its own token, nothing else served.
#![cfg(feature = "peers")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Router;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::proxy::{router, App, Secret};

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
         [streams.laptop]\ntoken_sha256 = \"{}\"\nchain = [\"zen-a\", \"zen-b\"]\n\
         [admin]\nlisten = \"127.0.0.1:0\"\ntoken_sha256 = \"{}\"\n\
         [plugins.metrics]\n\
         [peers]\nenabled = true\npublish = true\nlisten = \"{peer_addr}\"\n\
         peer_token_sha256 = \"{}\"\ninstance = \"ct-test\"\n{peers_extra}",
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

impl Rig {
    async fn new(peers_extra: &str) -> Self {
        let upstream = limited_upstream().await;
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

    async fn health(&self) -> serde_json::Value {
        let (code, body) = self.peer_get("/v1/health", Some(PEER_TOKEN)).await;
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }
}

#[tokio::test]
async fn only_the_peer_token_opens_the_peer_listener() {
    let rig = Rig::new("").await;
    for token in [
        None,
        Some(ADMIN_TOKEN),
        Some(STREAM_TOKEN),
        Some("not-a-token"),
    ] {
        let (code, body) = rig.peer_get("/v1/health", token).await;
        assert_eq!(code, 401, "{token:?}");
        assert!(body.is_empty(), "a refusal has no body");
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
        "/metrics",
        "/v1/usage",
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
    for path in ["/reload?dry_run=true", "/keys/enable?key=zen-a"] {
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
        "/metrics",
        "/v1/usage",
        "/reload",
        "/__reiki/healthz",
        "/zen/chat/completions",
    ] {
        for token in [None, Some(PEER_TOKEN), Some(ADMIN_TOKEN)] {
            let (code, _) = rig.peer_get(path, token).await;
            assert_eq!(code, 404, "{path} {token:?}");
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
    assert_eq!(doc["instance"], "ct-test");
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
    for secret in [
        "laptop",
        "zen-a",
        "/unused",
        "upstream-zen-a",
        &sha(PEER_TOKEN),
        &rig.upstream,
    ] {
        assert!(!text.contains(secret), "`{secret}` leaked: {text}");
    }
}

#[tokio::test]
async fn key_labels_appear_only_when_exposed() {
    let rig = Rig::new("expose_key_labels = true\n").await;
    let doc = rig.health().await;
    let keys = doc["upstreams"][0]["keys"].as_array().unwrap();
    let names: Vec<&str> = keys.iter().map(|k| k["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["zen-a", "zen-b"]);
    assert!(keys.iter().all(|k| k["state"] == "available"));
    assert!(!doc.to_string().contains("laptop"), "streams stay hidden");
}

#[tokio::test]
async fn failed_peer_logins_are_throttled_per_address() {
    let rig = Rig::new("").await;
    for _ in 0..10 {
        assert_eq!(rig.peer_get("/v1/health", Some("wrong")).await.0, 401);
    }
    let res = rig
        .client
        .get(format!("{}/v1/health", rig.peer))
        .bearer_auth(PEER_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429, "locked out even with the right token");
    assert!(res.headers().contains_key("retry-after"));
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
}

#[tokio::test]
async fn turning_publishing_off_on_reload_closes_the_route_and_asks_for_a_restart() {
    let rig = Rig::new("").await;
    let dir = std::env::temp_dir().join(format!("reiki-peers-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["zen-a", "zen-b"] {
        std::fs::write(dir.join(name), format!("upstream-{name}\n")).unwrap();
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
    assert_eq!(zen["soonest_cooldown_seconds"], 300, "{zen}");
    for key in zen["keys"].as_array().unwrap() {
        assert_eq!(key["cooldown_seconds"], 300, "{key}");
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
        "[[peers.tokens]]\nname = \"ct-5\"\ntoken_sha256 = \"{}\"\n",
        sha(second)
    ))
    .await;
    for token in [PEER_TOKEN, second] {
        let (code, body) = rig.peer_get("/v1/health", Some(token)).await;
        assert_eq!(code, 200, "{token}: {body}");
        assert!(
            !body.contains("ct-5"),
            "token names are never published: {body}"
        );
    }
    for token in [ADMIN_TOKEN, STREAM_TOKEN, "not-a-peer-token"] {
        let (code, _) = rig.peer_get("/v1/health", Some(token)).await;
        assert_eq!(code, 401, "{token}");
    }
}

#[tokio::test]
async fn a_second_peer_token_equal_to_another_credential_is_refused() {
    let addr: SocketAddr = "127.0.0.1:4999".parse().unwrap();
    for (reused, needle) in [
        (
            ADMIN_TOKEN,
            "peer token `ct-5` is the same as the admin token",
        ),
        (
            STREAM_TOKEN,
            "peer token `ct-5` is the same as the token of stream",
        ),
        (
            PEER_TOKEN,
            "peer token `ct-5` is the same as peer token `default`",
        ),
    ] {
        let text = config_text(
            "http://127.0.0.1:9",
            addr,
            &format!(
                "[[peers.tokens]]\nname = \"ct-5\"\ntoken_sha256 = \"{}\"\n",
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
        "{doc}"
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
    assert_eq!(sessions["active_sessions"], 2, "{sessions}");
    assert_eq!(sessions["window_seconds"], 300);
    assert!(
        sessions.get("streams").is_none(),
        "stream names stay hidden"
    );
    let text = doc.to_string();
    assert!(
        !text.contains("ses_alpha") && !text.contains("laptop"),
        "{text}"
    );
}

#[tokio::test]
async fn per_stream_sessions_need_key_labels_too() {
    let rig = Rig::new("expose_host_stats = true\nexpose_key_labels = true\n").await;
    rig.send(Some("ses_alpha")).await;
    let doc = rig.health().await;
    assert_eq!(
        doc["sessions"]["streams"],
        serde_json::json!([{"stream": "laptop", "active_sessions": 1}])
    );
}
