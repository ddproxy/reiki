//! The admin listener's two credentials: the full admin token opens every route, the optional
//! read-only token opens the read routes only, and every refusal uses one JSON envelope.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{App, load_keys, router};
use serde_json::Value;

mod common;
use common::serve;
use common::write_key;

const ADMIN_TOKEN: &str = "admin-token";
const READ_TOKEN: &str = "read-only-token";
const WRONG_TOKEN: &str = "wrong-token";
const STREAM_TOKEN: &str = "stream-token";
const ACTOR: &str = "ops-runner";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// The token credentials a request can present.
#[derive(Clone, Copy)]
enum Caller {
    Full,
    ReadOnly,
    Wrong,
    None,
}

impl Caller {
    fn token(self) -> Option<&'static str> {
        match self {
            Self::Full => Some(ADMIN_TOKEN),
            Self::ReadOnly => Some(READ_TOKEN),
            Self::Wrong => Some(WRONG_TOKEN),
            Self::None => None,
        }
    }
}

struct Rig {
    dir: PathBuf,
    config_path: PathBuf,
    admin: String,
    relay: String,
    client: reqwest::Client,
}

fn config_text(dir: &std::path::Path, mock_url: &str) -> String {
    format!(
        "[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n\
         [keys.a]\nupstream = \"zen\"\nfile = \"{}\"\n\
         [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n\
         [plugins.metrics]\n\
         [admin]\nlisten = \"127.0.0.1:41198\"\ntoken_sha256 = \"{}\"\nread_token_sha256 = \"{}\"\n",
        dir.join("key-a").display(),
        reiki::auth::sha256_hex(STREAM_TOKEN),
        reiki::auth::sha256_hex(ADMIN_TOKEN),
        reiki::auth::sha256_hex(READ_TOKEN),
    )
}

impl Rig {
    async fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("reiki-admin-tokens-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_key(dir.join("key-a"), "upstream-secret-a\n");
        // Every relayed request is answered 429, so each one lands in the failure ring.
        let mock_url = serve(Router::new().fallback(|_: Request| async {
            let mut response = Response::new(Body::from("{\"error\":\"mock\"}"));
            *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("120"));
            response
        }))
        .await;
        let text = config_text(&dir, &mock_url);
        let config = Config::parse(&text).unwrap();
        let keys = load_keys(&config).unwrap();
        let plugins = PluginChain::build(&config.plugins, &reiki::plugins::builtin()).unwrap();
        let app = App::with_plugins(
            config,
            keys,
            Policy::default(),
            Duration::from_secs(5),
            plugins,
        );
        let config_path = dir.join("reiki.toml");
        std::fs::write(&config_path, &text).unwrap();
        app.set_config_path(config_path.clone());
        let relay = serve(router(Arc::clone(&app))).await;
        let admin = serve(reiki::admin::router(Arc::clone(&app))).await;
        Self {
            dir,
            config_path,
            admin,
            relay,
            client: reqwest::Client::new(),
        }
    }

    /// One relayed request, which the mock answers 429.
    async fn fail_once(&self) {
        let status = self
            .client
            .post(format!("{}/zen/chat", self.relay))
            .bearer_auth(STREAM_TOKEN)
            .body("{}")
            .send()
            .await
            .unwrap()
            .status();
        assert_ne!(status, 200, "the mock refuses every request");
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        caller: Caller,
        actor: Option<&str>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.admin);
        let mut request = match method {
            "POST" => self.client.post(url),
            _ => self.client.get(url),
        };
        if let Some(token) = caller.token() {
            request = request.bearer_auth(token);
        }
        if let Some(actor) = actor {
            request = request.header("x-reiki-actor", actor);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, body)
    }

    async fn get(&self, path: &str, caller: Caller) -> (u16, Value) {
        self.call("GET", path, caller, None).await
    }

    async fn post(&self, path: &str, caller: Caller) -> (u16, Value) {
        self.call("POST", path, caller, None).await
    }

    fn write_config(&self, text: &str) {
        std::fs::write(&self.config_path, text).unwrap();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The envelope every refusal uses: `ok` false, a stable code and a message.
fn assert_envelope(body: &Value, code: &str) {
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{body}"
    );
}

const READ_ROUTES: [&str; 4] = [
    "/v1/status",
    "/v1/failures",
    "/v1/stream-console",
    "/v1/openapi.json",
];

#[tokio::test]
async fn the_full_token_opens_every_read_route() {
    let rig = Rig::new().await;
    for path in READ_ROUTES.iter().chain(&["/v1/metrics"]) {
        let (status, _) = rig.get(path, Caller::Full).await;
        assert_eq!(status, 200, "{path} with the full token");
    }
}

#[tokio::test]
async fn the_read_only_token_opens_the_read_routes_and_nothing_else() {
    let rig = Rig::new().await;
    for path in READ_ROUTES.iter().chain(&["/v1/metrics"]) {
        let (status, _) = rig.get(path, Caller::ReadOnly).await;
        assert_eq!(status, 200, "{path} with the read-only token");
    }
    let (status, body) = rig.post("/v1/reload", Caller::ReadOnly).await;
    assert_eq!(status, 403, "{body}");
    assert_envelope(&body, "read_only_token");
    let (status, body) = rig.post("/v1/keys/enable?key=a", Caller::ReadOnly).await;
    assert_eq!(status, 403, "{body}");
    assert_envelope(&body, "read_only_token");
    let (_, status_body) = rig.get("/v1/status", Caller::Full).await;
    assert_eq!(
        status_body["config"]["generation"], 0,
        "a refused reload changed nothing"
    );
}

#[tokio::test]
async fn a_missing_or_wrong_token_gets_the_same_401_on_every_route() {
    let rig = Rig::new().await;
    for (method, path) in [
        ("GET", "/v1/status"),
        ("GET", "/v1/failures"),
        ("GET", "/v1/stream-console"),
        ("GET", "/v1/openapi.json"),
        ("GET", "/v1/metrics"),
        ("POST", "/v1/reload"),
        ("POST", "/v1/keys/enable?key=a"),
    ] {
        let (missing, missing_body) = rig.call(method, path, Caller::None, None).await;
        let (wrong, wrong_body) = rig.call(method, path, Caller::Wrong, None).await;
        assert_eq!(missing, 401, "{method} {path} without a token");
        assert_eq!(wrong, 401, "{method} {path} with a wrong token");
        assert_eq!(missing_body, wrong_body, "{method} {path}: one 401 shape");
        assert_envelope(&missing_body, "unauthorized");
    }
}

#[tokio::test]
async fn the_full_token_reloads_and_enables_keys() {
    let rig = Rig::new().await;
    let (status, body) = rig.post("/v1/reload?dry_run=true", Caller::Full).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dry_run"], true);
    let (status, body) = rig.post("/v1/reload", Caller::Full).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["generation"], 1);
    let (status, body) = rig.post("/v1/keys/enable?key=nope", Caller::Full).await;
    assert_eq!(status, 404, "{body}");
    assert_envelope(&body, "unknown_key");
    let (status, body) = rig.post("/v1/keys/enable?key=a", Caller::Full).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["key"], "a");
}

#[tokio::test]
async fn an_actor_label_never_changes_what_a_token_may_do() {
    let rig = Rig::new().await;
    let (status, body) = rig
        .call("POST", "/v1/reload", Caller::ReadOnly, Some(ACTOR))
        .await;
    assert_eq!(status, 403, "{body}");
    let (status, body) = rig
        .call("POST", "/v1/reload", Caller::Full, Some("odd label;<x>"))
        .await;
    assert_eq!(status, 200, "a sanitised label is only logged: {body}");
}

#[tokio::test]
async fn an_unknown_admin_route_uses_the_envelope() {
    let rig = Rig::new().await;
    let (status, body) = rig.get("/v1/nope", Caller::Full).await;
    assert_eq!(status, 404);
    assert_envelope(&body, "not_found");
}

#[tokio::test]
async fn a_bad_request_uses_the_envelope() {
    let rig = Rig::new().await;
    let (status, body) = rig.post("/v1/reload?dry_run=maybe", Caller::Full).await;
    assert_eq!(status, 400);
    assert_envelope(&body, "bad_dry_run");
    let (status, body) = rig.get("/v1/failures?limit=0", Caller::Full).await;
    assert_eq!(status, 400);
    assert_envelope(&body, "bad_request");
}

#[tokio::test]
async fn the_read_only_token_gets_no_failure_snippets() {
    let rig = Rig::new().await;
    rig.fail_once().await;

    let (_, full) = rig.get("/v1/failures", Caller::Full).await;
    let full_row = &full["failures"][0];
    assert!(
        full_row["note"].is_string(),
        "the full token sees the snippet: {full}"
    );

    let (status, read) = rig.get("/v1/failures", Caller::ReadOnly).await;
    assert_eq!(status, 200);
    let read_row = &read["failures"][0];
    assert!(
        read_row["note"].is_null(),
        "no free text for read-only: {read}"
    );
    assert_eq!(read_row["class"], full_row["class"]);
    assert_eq!(read_row["action"], full_row["action"]);
    assert_eq!(read_row["status"], full_row["status"]);
    assert_eq!(read_row["key"], "a");
}

#[tokio::test]
async fn the_read_only_token_gets_a_code_for_a_failed_reload() {
    let rig = Rig::new().await;
    rig.write_config("this is not = [valid toml");
    let (status, body) = rig.post("/v1/reload", Caller::Full).await;
    assert_eq!(status, 422, "{body}");
    assert_envelope(&body, "config_refused");
    let message = body["error"]["message"].as_str().unwrap().to_owned();

    let (_, full) = rig.get("/v1/status", Caller::Full).await;
    assert_eq!(full["config"]["last_reload"]["error"], message.as_str());

    let (status, read) = rig.get("/v1/status", Caller::ReadOnly).await;
    assert_eq!(status, 200);
    assert_eq!(read["config"]["last_reload"]["ok"], false);
    assert_eq!(read["config"]["last_reload"]["error"], "reload_refused");
    assert!(
        !read.to_string().contains(&message),
        "the read-only status must not carry the refusal text"
    );
}

#[tokio::test]
async fn the_read_only_token_must_differ_from_the_admin_token() {
    // Equal hashes are refused at config load, so one token can never open the mutating routes
    // as the read-only class.
    let text = config_text(std::path::Path::new("/tmp"), "http://127.0.0.1:9");
    let equal = text.replace(
        &reiki::auth::sha256_hex(READ_TOKEN),
        &reiki::auth::sha256_hex(ADMIN_TOKEN),
    );
    let error = Config::parse(&equal).unwrap_err();
    assert!(
        error.to_string().contains("must differ"),
        "a clear startup error: {error}"
    );
}
