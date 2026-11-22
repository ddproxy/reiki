//! Real HTTP integration tests for the opt-in `[stream_admin]` API.
//!
//! The rig builds the same App, manager and listeners the binary builds, with each port taken
//! from an ephemeral bind. No assertion message contains a token or a stream secret.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::Value;

use reiki::chain::Policy;
use reiki::config::Config;
use reiki::plugins::PluginChain;
use reiki::proxy::{App, load_keys, router};

const ADMIN_TOKEN: &str = "admin-token-for-stream-admin-tests";
const STREAM_ADMIN_TOKEN: &str = "stream-manager-token-for-stream-admin-tests";
const CONFIRM_TOKEN: &str = "second-party-token-for-stream-admin-tests";
const ACTOR: &str = "operator-id-for-stream-admin-tests";
const STATIC_TOKEN: &str = "static-stream-token-for-stream-admin-tests";
const PEER_TOKEN: &str = "peer-token-for-stream-admin-tests";
const WRONG_TOKEN_FOR_LOG_TEST: &str = "unique-wrong-credential-for-log-check";
const STORE_MESSAGE: &str =
    "stream management is unavailable; the audit log or state file could not be written";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn sha(value: &str) -> String {
    reiki::auth::sha256_hex(value)
}

#[derive(Clone)]
struct Options {
    enabled: bool,
    two_party: bool,
    lockout_threshold: u32,
    rpm: u32,
    max_body_bytes: usize,
    max_streams: u32,
    max_tokens_per_stream: u32,
    #[cfg(feature = "peers")]
    peers: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            enabled: true,
            two_party: false,
            lockout_threshold: 5,
            rpm: 60,
            max_body_bytes: 1024,
            max_streams: 4,
            max_tokens_per_stream: 3,
            #[cfg(feature = "peers")]
            peers: false,
        }
    }
}

struct Reply {
    status: u16,
    body: String,
    headers: HeaderMap,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// Serves on an already-bound listener, so a config can name that exact loopback address.
#[cfg(feature = "peers")]
async fn serve_on(listener: tokio::net::TcpListener, routes: Router) -> String {
    let addr = listener.local_addr().unwrap();
    tokio::spawn(reiki::serve::serve(
        listener,
        routes,
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

struct Rig {
    dir: PathBuf,
    text: String,
    mock_url: String,
    app: Arc<App>,
    relay: String,
    admin: String,
    #[cfg(feature = "peers")]
    #[cfg_attr(not(feature = "top"), allow(dead_code))]
    peer: Option<String>,
    client: reqwest::Client,
    owns_dir: bool,
    opts: Options,
}

impl Rig {
    async fn new(opts: Options) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "reiki-stream-admin-integration-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for key in ["zen-a", "zen-b", "zen-c"] {
            common::write_key(dir.join(format!("key-{key}")), format!("upstream-{key}\n"));
        }
        let mock = Router::new().fallback(any(|body: String| async move { body }));
        let mock_url = common::serve(mock).await;
        let text = Self::config_text(&dir, &mock_url, &opts);
        Self::assemble(dir, text, mock_url, opts, true).await
    }

    /// Rebuilds the same listeners from the same files, as a restart would.
    async fn restart(&self) -> Self {
        Self::assemble(
            self.dir.clone(),
            self.text.clone(),
            self.mock_url.clone(),
            self.opts.clone(),
            false,
        )
        .await
    }

    fn config_text(dir: &Path, mock_url: &str, opts: &Options) -> String {
        // The config only needs a literal loopback address to pass validation; the admin router
        // is served on a separate ephemeral socket by `common::serve`.
        let admin_listen = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            drop(listener);
            addr
        };
        #[cfg(feature = "peers")]
        let peer_block = if opts.peers {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            format!(
                "[peers]\nenabled = true\npublish = true\nlisten = \"{addr}\"\n\
                 peer_token_sha256 = \"{}\"\ninstance = \"stream-admin-test\"\nexpose_usage = true\n",
                sha(PEER_TOKEN)
            )
        } else {
            String::new()
        };
        #[cfg(not(feature = "peers"))]
        let peer_block = String::new();
        let stream_admin = if opts.enabled {
            let two_party = if opts.two_party {
                format!("require_two_party = [\"{}\"]\n", sha(CONFIRM_TOKEN))
            } else {
                String::new()
            };
            format!(
                "[stream_admin]\ntoken_sha256 = \"{}\"\nactor_id_sha256 = \"{}\"\n\
                 state_file = \"stream-admin.json\"\naudit_log = \"audit.jsonl\"\n\
                 max_streams = {}\nmax_tokens_per_stream = {}\n\
                 default_ttl = 3600\nmax_ttl = 86400\nmax_body_bytes = {}\n\
                 lockout_threshold = {}\nlockout_secs = 900\nrpm = {}\n\
                 allowed_listen = [\"{}\"]\n{two_party}\
                 [stream_admin.allowed_keys.zen-a]\nassignable = true\n\
                 [stream_admin.allowed_keys.zen-b]\nassignable = true\n\
                 [stream_admin.ceilings]\nrpm = 100\ntpm = 100000\nconcurrent = 10\n\
                 daily_requests = 1000\ndaily_tokens = 100000\n\
                 monthly_requests = 10000\nmonthly_tokens = 1000000\n",
                sha(STREAM_ADMIN_TOKEN),
                sha(ACTOR),
                opts.max_streams,
                opts.max_tokens_per_stream,
                opts.max_body_bytes,
                opts.lockout_threshold,
                opts.rpm,
                admin_listen,
            )
        } else {
            String::new()
        };
        format!(
            "[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n\
             [keys.zen-a]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.zen-b]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [keys.zen-c]\nupstream = \"zen\"\nfile = \"{}\"\n\
             [streams.static-s]\ntoken_sha256 = \"{}\"\nchain = [\"zen-a\"]\n\
             [admin]\nlisten = \"{admin_listen}\"\ntoken_sha256 = \"{}\"\n\
             {stream_admin}{peer_block}",
            dir.join("key-zen-a").display(),
            dir.join("key-zen-b").display(),
            dir.join("key-zen-c").display(),
            sha(STATIC_TOKEN),
            sha(ADMIN_TOKEN),
        )
    }

    async fn assemble(
        dir: PathBuf,
        text: String,
        mock_url: String,
        opts: Options,
        owns_dir: bool,
    ) -> Self {
        let config_path = dir.join("reiki.toml");
        std::fs::write(&config_path, &text).unwrap();
        let config = Config::parse(&text).unwrap();
        let keys = load_keys(&config).unwrap();
        let settings = config.stream_admin.clone();
        let app = App::with_plugins(
            config,
            keys,
            Policy::default(),
            Duration::from_secs(5),
            PluginChain::default(),
        );
        app.set_config_path(config_path);
        if let Some(settings) = &settings {
            let manager = reiki::stream_admin::Manager::new(settings, &dir);
            app.set_stream_admin(Arc::clone(&manager));
            manager.reload_from_disk();
        }
        let relay = common::serve(router(Arc::clone(&app))).await;
        let admin = common::serve(reiki::admin::router(Arc::clone(&app))).await;
        #[cfg(feature = "peers")]
        let peer = if opts.peers {
            let (listener, _) = bind().await;
            Some(serve_on(listener, reiki::peers::router(Arc::clone(&app))).await)
        } else {
            None
        };
        Self {
            dir,
            text,
            mock_url,
            app,
            relay,
            admin,
            #[cfg(feature = "peers")]
            peer,
            client: reqwest::Client::new(),
            owns_dir,
            opts,
        }
    }

    fn audit_path(&self) -> PathBuf {
        self.dir.join("audit.jsonl")
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("stream-admin.json")
    }

    fn head_path(&self) -> PathBuf {
        self.audit_path().with_extension("head")
    }

    async fn reload(&self) {
        let config = Config::parse(&self.text).unwrap();
        let keys = load_keys(&config).unwrap();
        let app = Arc::clone(&self.app);
        tokio::task::spawn_blocking(move || app.reload(config, keys))
            .await
            .unwrap();
    }

    // Test helper: keeping method/base/body/auth/idempotency/headers separate makes each call site
    // self-documenting, at the cost of one argument more than clippy's default threshold.
    #[allow(clippy::too_many_arguments)]
    async fn call(
        &self,
        method: Method,
        base: &str,
        path: &str,
        body: Option<&str>,
        authorization: Option<&str>,
        idempotency: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Reply {
        let mut request = self.client.request(method, format!("{base}{path}"));
        if let Some(value) = authorization {
            request = request.header("authorization", value);
        }
        if let Some(key) = idempotency {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            if !extra
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                request = request.header("content-type", "application/json");
            }
            request = request.body(body.to_owned());
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        let headers = response.headers().clone();
        Reply {
            status: response.status().as_u16(),
            body: response.text().await.unwrap(),
            headers,
        }
    }

    async fn stream_call(
        &self,
        method: Method,
        path: &str,
        body: Option<&str>,
        idempotency: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Reply {
        let auth = format!("Bearer {STREAM_ADMIN_TOKEN}");
        self.call(
            method,
            &self.admin,
            path,
            body,
            Some(&auth),
            idempotency,
            extra,
        )
        .await
    }

    async fn admin_get(&self, path: &str) -> Reply {
        let auth = format!("Bearer {ADMIN_TOKEN}");
        self.call(Method::GET, &self.admin, path, None, Some(&auth), None, &[])
            .await
    }

    async fn send_relay(&self, stream_token: &str) -> u16 {
        self.call(
            Method::POST,
            &self.relay,
            "/zen/chat",
            Some("{\"q\":\"hello\"}"),
            Some(&format!("Bearer {stream_token}")),
            None,
            &[],
        )
        .await
        .status
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if self.owns_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn assert_no_credential(reply: &Reply, secrets: &[&str]) {
    let headers: String = reply
        .headers
        .iter()
        .map(|(name, value)| format!("{}:{}", name.as_str(), value.to_str().unwrap_or("<binary>")))
        .collect();
    for secret in secrets {
        assert!(
            !reply.body.contains(secret),
            "a credential leaked into a body"
        );
        assert!(
            !headers.contains(secret),
            "a credential leaked into headers"
        );
    }
}

fn response_fingerprint(reply: &Reply) -> (u16, String, Vec<(String, String)>) {
    let headers = reply
        .headers
        .iter()
        .filter(|(name, _)| name.as_str() != "date" && name.as_str() != "x-reiki-version")
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or("<binary>").to_owned(),
            )
        })
        .collect();
    (reply.status, reply.body.clone(), headers)
}

fn create_body(name: &str, reason: &str) -> String {
    serde_json::json!({
        "name": name,
        "key": "zen-a",
        "reason": reason,
        "limits": {"rpm": 10},
    })
    .to_string()
}

fn created_token(reply: &Reply) -> String {
    let json = reply.json();
    assert!(
        json.get("token").is_some(),
        "the initial token must be shown once"
    );
    json["token"].as_str().unwrap().to_owned()
}

fn audit_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn without_the_feature_the_stream_routes_are_unavailable() {
    let options = Options {
        enabled: false,
        ..Options::default()
    };
    let rig = Rig::new(options).await;
    let auth = format!("Bearer {STREAM_ADMIN_TOKEN}");
    let cases: Vec<(Method, &str, Option<String>)> = vec![
        (Method::GET, "/v1/streams", None),
        (
            Method::POST,
            "/v1/streams/create",
            Some(create_body("dyn", "setup")),
        ),
        (Method::GET, "/v1/streams/dyn", None),
        (
            Method::PUT,
            "/v1/streams/dyn",
            Some(serde_json::json!({"reason": "setup", "rpm": 1}).to_string()),
        ),
        (
            Method::DELETE,
            "/v1/streams/dyn",
            Some(serde_json::json!({"reason": "setup"}).to_string()),
        ),
        (
            Method::POST,
            "/v1/streams/dyn/tokens",
            Some(serde_json::json!({"reason": "setup"}).to_string()),
        ),
        (
            Method::POST,
            "/v1/streams/dyn/revoke",
            Some(serde_json::json!({"reason": "setup"}).to_string()),
        ),
        (
            Method::POST,
            "/v1/streams/dyn/tokens/abc/revoke",
            Some(serde_json::json!({"reason": "setup"}).to_string()),
        ),
    ];
    for (method, path, body) in cases {
        let reply = rig
            .call(
                method,
                &rig.admin,
                path,
                body.as_deref(),
                Some(&auth),
                Some("disabled-check"),
                &[],
            )
            .await;
        assert!(
            matches!(reply.status, 401 | 404),
            "a disabled stream route must not be usable"
        );
        assert!(
            reply.body.is_empty(),
            "a disabled route must not describe itself"
        );
    }
    assert!(
        !rig.state_path().exists(),
        "no state file without the feature"
    );
    assert!(
        !rig.audit_path().exists(),
        "no audit file without the feature"
    );

    let openapi = rig.admin_get("/v1/openapi.json").await;
    assert_eq!(openapi.status, 200, "the ordinary admin API still works");
    let paths = openapi.json()["paths"].as_object().unwrap().clone();
    assert!(
        paths.keys().all(|path| !path.starts_with("/v1/streams")),
        "a disabled instance must not document stream routes"
    );
    let status = rig.admin_get("/v1/status").await.json();
    assert!(
        status.get("stream_admin").is_none(),
        "no stream status when disabled"
    );
    assert_eq!(
        rig.send_relay(STATIC_TOKEN).await,
        200,
        "static streams still work"
    );
}

#[tokio::test]
async fn bad_credentials_are_indistinguishable_and_lock_out_the_source() {
    let rig = Rig::new(Options::default()).await;
    let wrong = format!("Bearer {WRONG_TOKEN_FOR_LOG_TEST}");
    let missing = rig
        .call(
            Method::GET,
            &rig.admin,
            "/v1/streams",
            None,
            None,
            None,
            &[],
        )
        .await;
    let wrong_token = rig
        .call(
            Method::GET,
            &rig.admin,
            "/v1/streams",
            None,
            Some(&wrong),
            None,
            &[],
        )
        .await;
    let malformed = rig
        .call(
            Method::GET,
            &rig.admin,
            "/v1/streams",
            None,
            Some("Basic not-a-bearer-credential"),
            None,
            &[],
        )
        .await;
    for reply in [&missing, &wrong_token, &malformed] {
        assert_eq!(reply.status, 401, "every refused credential looks the same");
        assert!(
            reply.body.is_empty(),
            "a refusal must not explain the reason"
        );
        assert_no_credential(reply, &[WRONG_TOKEN_FOR_LOG_TEST, STREAM_ADMIN_TOKEN]);
    }
    assert_eq!(
        response_fingerprint(&missing),
        response_fingerprint(&wrong_token),
        "missing and wrong credentials must be indistinguishable"
    );
    assert_eq!(
        response_fingerprint(&missing),
        response_fingerprint(&malformed),
        "malformed credentials must be indistinguishable"
    );

    let lockout = Rig::new(Options {
        lockout_threshold: 2,
        rpm: 10,
        ..Options::default()
    })
    .await;
    for _ in 0..2 {
        let refused = lockout
            .call(
                Method::GET,
                &lockout.admin,
                "/v1/streams",
                None,
                Some(&wrong),
                None,
                &[],
            )
            .await;
        assert_eq!(refused.status, 401, "the wrong token is refused");
    }
    let locked = lockout
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await;
    assert_eq!(
        locked.status, 429,
        "the correct token is locked out after two failures"
    );
    assert_no_credential(&locked, &[STREAM_ADMIN_TOKEN]);
    let records = reiki::stream_admin::audit_head(&rig.audit_path(), 16).unwrap();
    assert!(
        records.iter().any(|record| record.outcome == "denied"),
        "a denied attempt must be audited"
    );
    let audit = std::fs::read_to_string(rig.audit_path()).unwrap();
    assert!(
        !audit.contains(WRONG_TOKEN_FOR_LOG_TEST),
        "an audit must not hold a token"
    );
}

#[tokio::test]
async fn idempotency_replays_hide_tokens_and_conflict_on_a_different_body() {
    let rig = Rig::new(Options::default()).await;
    let body = create_body("dyn-a", "first setup");
    let first = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("create-key-a"),
            &[],
        )
        .await;
    assert_eq!(first.status, 200, "create must succeed");
    let token = created_token(&first);

    let replay = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("create-key-a"),
            &[],
        )
        .await;
    assert_eq!(replay.status, 200, "the same key must replay");
    assert_eq!(
        replay.json()["token_id"],
        first.json()["token_id"],
        "same token id"
    );
    assert!(
        replay.json().get("token").is_none(),
        "a replay must not reshow a token"
    );
    assert_no_credential(&replay, &[&token]);

    let changed = create_body("dyn-b", "a different setup");
    let conflict = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&changed),
            Some("create-key-a"),
            &[],
        )
        .await;
    assert_eq!(
        conflict.status, 409,
        "the key is bound to the original body"
    );
    assert!(
        conflict.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflicts with existing state"),
        "the refusal must be the fixed conflict message"
    );

    let missing_key = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&create_body("dyn-c", "missing key")),
            None,
            &[],
        )
        .await;
    assert_eq!(
        missing_key.status, 400,
        "a mutating request needs Idempotency-Key"
    );
    let no_reason = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some("{\"name\":\"dyn-c\",\"key\":\"zen-a\"}"),
            Some("create-key-c"),
            &[],
        )
        .await;
    assert_eq!(no_reason.status, 400, "a mutating request needs a reason");
    let empty_reason = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&create_body("dyn-c", "")),
            Some("create-key-c"),
            &[],
        )
        .await;
    assert_eq!(empty_reason.status, 400, "an empty reason is refused");
    let streams = rig
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await
        .json();
    assert_eq!(
        streams["streams"].as_array().unwrap().len(),
        1,
        "nothing extra was created"
    );

    let token_body = serde_json::json!({"reason": "rotate"}).to_string();
    let added = rig
        .stream_call(
            Method::POST,
            "/v1/streams/dyn-a/tokens",
            Some(&token_body),
            Some("add-key-a"),
            &[],
        )
        .await;
    assert_eq!(added.status, 200, "a token may be added");
    let added_token = created_token(&added);
    let replay_add = rig
        .stream_call(
            Method::POST,
            "/v1/streams/dyn-a/tokens",
            Some(&token_body),
            Some("add-key-a"),
            &[],
        )
        .await;
    assert_eq!(replay_add.status, 200, "the add key replays");
    assert!(
        replay_add.json().get("token").is_none(),
        "the replay must hide the new token"
    );
    assert_no_credential(&replay_add, &[&added_token]);
}

#[tokio::test]
async fn strict_bodies_and_configured_ceilings_are_enforced() {
    let rig = Rig::new(Options::default()).await;
    let unknown = serde_json::json!({
        "name": "dyn-a", "key": "zen-a", "reason": "setup", "surprise": 1
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&unknown),
            Some("strict-unknown"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 400, "unknown fields are refused");

    let oversized = serde_json::json!({
        "name": "dyn-a", "key": "zen-a", "reason": "x".repeat(1100)
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&oversized),
            Some("strict-large"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 413, "oversized bodies are refused");

    let reply = rig
        .call(
            Method::POST,
            &rig.admin,
            "/v1/streams/create",
            Some(&create_body("dyn-a", "wrong content type")),
            Some(&format!("Bearer {STREAM_ADMIN_TOKEN}")),
            Some("strict-content"),
            &[("content-type", "text/plain")],
        )
        .await;
    assert_eq!(reply.status, 415, "JSON content type is required");

    let unassignable = serde_json::json!({
        "name": "dyn-a", "key": "zen-c", "reason": "setup"
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&unassignable),
            Some("strict-key"),
            &[],
        )
        .await;
    assert_eq!(
        reply.status, 400,
        "only configured assignable keys may be used"
    );

    let over_ceiling = serde_json::json!({
        "name": "dyn-a", "key": "zen-a", "reason": "setup", "limits": {"rpm": 101}
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&over_ceiling),
            Some("strict-ceiling"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 422, "limits above the ceiling are refused");

    let bad_name = serde_json::json!({
        "name": "../etc", "key": "zen-a", "reason": "setup"
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&bad_name),
            Some("strict-name"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 409, "an invalid stream name is refused");

    let static_collision = serde_json::json!({
        "name": "static-s", "key": "zen-a", "reason": "setup"
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&static_collision),
            Some("strict-static"),
            &[],
        )
        .await;
    assert_eq!(
        reply.status, 409,
        "a configured stream name cannot be reused"
    );

    let body = create_body("dyn-a", "setup");
    let reply = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("strict-good"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "a valid request still succeeds");
    let duplicate = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("strict-duplicate"),
            &[],
        )
        .await;
    assert_eq!(duplicate.status, 409, "a duplicate dynamic name conflicts");

    let bad_update = serde_json::json!({
        "reason": "rotate", "key": "zen-c"
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::PUT,
            "/v1/streams/dyn-a",
            Some(&bad_update),
            Some("strict-update-key"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 400, "update rejects an unassignable key");
    let bad_limits = serde_json::json!({
        "reason": "raise", "limits": {"tpm": 100001}
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::PUT,
            "/v1/streams/dyn-a",
            Some(&bad_limits),
            Some("strict-update-limit"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 422, "update rejects a ceiling violation");
    let no_change = serde_json::json!({"reason": "nothing"}).to_string();
    let reply = rig
        .stream_call(
            Method::PUT,
            "/v1/streams/dyn-a",
            Some(&no_change),
            Some("strict-update-empty"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 400, "an update must change something");

    for ttl in [0_u64, 86_401_u64] {
        let token_body = serde_json::json!({"reason": "rotate", "ttl": ttl}).to_string();
        let reply = rig
            .stream_call(
                Method::POST,
                "/v1/streams/dyn-a/tokens",
                Some(&token_body),
                Some(&format!("strict-ttl-{ttl}")),
                &[],
            )
            .await;
        assert_eq!(
            reply.status, 422,
            "token TTL outside the configured range is refused"
        );
    }
    let streams = rig
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await
        .json();
    let entries = streams["streams"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "refused requests did not create a stream");
    assert_eq!(
        entries[0]["key"], "zen-a",
        "refused requests did not update the key"
    );
    assert_eq!(
        entries[0]["limits"]["rpm"], 10,
        "refused requests did not change limits"
    );
    assert_eq!(
        entries[0]["tokens"].as_array().unwrap().len(),
        1,
        "no extra tokens"
    );
}

#[tokio::test]
async fn the_full_lifecycle_returns_tokens_once_and_persists_hashes_only() {
    let rig = Rig::new(Options::default()).await;
    let body = create_body("dyn-a", "create for lifecycle");
    let created = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("lifecycle-create"),
            &[],
        )
        .await;
    assert_eq!(created.status, 200, "create must succeed");
    let token = created_token(&created);
    let token_hash = sha(&token);

    let list = rig
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await
        .json();
    let shown = rig
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await
        .json();
    assert_eq!(
        list["streams"].as_array().unwrap().len(),
        1,
        "one dynamic stream"
    );
    assert_eq!(shown["name"], "dyn-a", "show finds the created stream");
    assert_eq!(shown["key"], "zen-a", "show reports the assigned key");
    assert!(shown.get("token").is_none(), "show must not reveal a token");
    assert_no_credential(
        &rig.stream_call(Method::GET, "/v1/streams", None, None, &[])
            .await,
        &[&token],
    );

    let state = std::fs::read_to_string(rig.state_path()).unwrap();
    assert!(
        !state.contains(&token),
        "the state file must not contain a raw token"
    );
    assert!(
        !state.contains("rsk_"),
        "the state file must not contain token prefixes"
    );
    assert!(
        state.contains(&token_hash),
        "the state file stores the token hash"
    );
    let audit = std::fs::read_to_string(rig.audit_path()).unwrap();
    assert!(
        !audit.contains(&token),
        "the audit file must not contain a raw token"
    );

    let update = serde_json::json!({
        "reason": "move to the second key", "key": "zen-b", "limits": {"tpm": 500}
    })
    .to_string();
    let reply = rig
        .stream_call(
            Method::PUT,
            "/v1/streams/dyn-a",
            Some(&update),
            Some("lifecycle-update"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "update must succeed");
    let shown = rig
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await
        .json();
    assert_eq!(shown["key"], "zen-b", "the key was updated");
    assert_eq!(shown["limits"]["tpm"], 500, "the limits were updated");

    let token_body = serde_json::json!({"reason": "second client"}).to_string();
    let added = rig
        .stream_call(
            Method::POST,
            "/v1/streams/dyn-a/tokens",
            Some(&token_body),
            Some("lifecycle-add"),
            &[],
        )
        .await;
    assert_eq!(added.status, 200, "adding a token must succeed");
    let second_token = created_token(&added);
    let second_hash = sha(&second_token);
    let shown = rig
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await
        .json();
    assert_eq!(
        shown["tokens"].as_array().unwrap().len(),
        2,
        "both tokens are listed"
    );
    let state = std::fs::read_to_string(rig.state_path()).unwrap();
    assert!(
        state.contains(&second_hash),
        "the added token is stored as a hash"
    );
    assert!(
        !state.contains(&second_token),
        "the added token is not stored raw"
    );

    let first_id = created.json()["token_id"].as_str().unwrap().to_owned();
    let revoke = serde_json::json!({"reason": "first client left"}).to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            &format!("/v1/streams/dyn-a/tokens/{first_id}/revoke"),
            Some(&revoke),
            Some("lifecycle-revoke"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "revoking one token must succeed");
    let shown = rig
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await
        .json();
    assert_eq!(
        shown["tokens"].as_array().unwrap().len(),
        1,
        "one token was revoked"
    );
    assert_eq!(
        rig.send_relay(&second_token).await,
        200,
        "the surviving token still works"
    );

    let delete = serde_json::json!({"reason": "tenant removed"}).to_string();
    let reply = rig
        .stream_call(
            Method::DELETE,
            "/v1/streams/dyn-a",
            Some(&delete),
            Some("lifecycle-delete"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "delete must succeed");
    let shown = rig
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await;
    assert_eq!(shown.status, 404, "the deleted stream is gone");
    assert_eq!(
        rig.send_relay(&second_token).await,
        401,
        "deleted credentials stop working"
    );
    assert_eq!(
        rig.send_relay(STATIC_TOKEN).await,
        200,
        "static streams are unaffected"
    );
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.state_path()).unwrap()).unwrap();
    assert!(
        state["streams"].as_array().unwrap().is_empty(),
        "the state file is empty again"
    );
}

#[tokio::test]
async fn two_party_confirmation_requires_the_second_raw_token() {
    let options = Options {
        two_party: true,
        ..Options::default()
    };
    let rig = Rig::new(options).await;
    let body = create_body("dyn-a", "confirmed setup");
    let missing = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("confirm-missing"),
            &[],
        )
        .await;
    let wrong = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("confirm-wrong"),
            &[("x-confirm-token", "not-the-second-token")],
        )
        .await;
    for reply in [&missing, &wrong] {
        assert_eq!(reply.status, 401, "a mutation needs the second credential");
        assert!(
            reply.body.is_empty(),
            "a confirmation refusal reveals nothing"
        );
        assert_no_credential(reply, &[CONFIRM_TOKEN, STREAM_ADMIN_TOKEN]);
    }
    assert_eq!(
        response_fingerprint(&missing),
        response_fingerprint(&wrong),
        "a wrong confirmation is indistinguishable from a missing one"
    );
    let list = rig
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await;
    assert_eq!(list.status, 200, "reads do not need confirmation");
    assert_eq!(
        list.json()["streams"].as_array().unwrap().len(),
        0,
        "no mutation happened"
    );

    let confirmed = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("confirm-correct"),
            &[("x-confirm-token", CONFIRM_TOKEN)],
        )
        .await;
    assert_eq!(
        confirmed.status, 200,
        "the correct confirmation permits the mutation"
    );
    let token = created_token(&confirmed);
    assert_no_credential(&confirmed, &[CONFIRM_TOKEN]);
    assert_eq!(rig.send_relay(&token).await, 200, "the created token works");
    let records = reiki::stream_admin::audit_head(&rig.audit_path(), 16).unwrap();
    assert!(
        records.iter().any(|record| record.outcome == "denied"),
        "the confirmation refusal must be audited"
    );
    assert!(
        records.iter().any(|record| record.outcome == "success"),
        "the confirmed mutation must be audited"
    );
}

#[tokio::test]
async fn state_survives_a_reload_or_restart_and_bad_state_only_blocks_dynamic_streams() {
    let rig = Rig::new(Options::default()).await;
    let body = create_body("dyn-a", "persistent setup");
    let created = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("persist-create"),
            &[],
        )
        .await;
    assert_eq!(created.status, 200, "create must succeed");
    let token = created_token(&created);
    let token_id = created.json()["token_id"].as_str().unwrap().to_owned();
    assert_eq!(
        rig.send_relay(&token).await,
        200,
        "the token works before reload"
    );

    rig.reload().await;
    let shown = rig
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await;
    assert_eq!(shown.status, 200, "reload restores the stream");
    assert_eq!(
        shown.json()["tokens"][0]["id"],
        token_id,
        "the token id survives reload"
    );
    assert_eq!(
        rig.send_relay(&token).await,
        200,
        "the token still authenticates"
    );

    let restarted = rig.restart().await;
    let list = restarted
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await
        .json();
    assert_eq!(
        list["streams"].as_array().unwrap().len(),
        1,
        "restart reads the state file"
    );
    let shown = restarted
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await
        .json();
    assert_eq!(shown["key"], "zen-a", "the persisted key is restored");
    assert!(
        shown.get("token").is_none(),
        "restarted reads never reveal tokens"
    );
    assert_eq!(
        restarted.send_relay(&token).await,
        200,
        "the persisted token works"
    );

    std::fs::write(rig.state_path(), "{ not valid json").unwrap();
    restarted.reload().await;
    let list = restarted
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await
        .json();
    assert_eq!(
        list["streams"].as_array().unwrap().len(),
        0,
        "invalid state drops dynamic streams"
    );
    let shown = restarted
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await;
    assert_eq!(
        shown.status, 404,
        "a dynamic stream from invalid state is refused"
    );
    assert!(
        shown.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no such dynamic stream or token"),
        "the refusal is a fixed public message"
    );
    let status = restarted.admin_get("/v1/status").await.json();
    assert!(
        status["stream_admin"]["state_error"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "status explains that dynamic state is unavailable"
    );
    assert_eq!(
        restarted.send_relay(&token).await,
        401,
        "the old token cannot authenticate"
    );
    assert_eq!(
        restarted.send_relay(STATIC_TOKEN).await,
        200,
        "static streams stay available"
    );
}

#[tokio::test]
async fn audit_records_mutations_and_the_head_sidecar_detects_tampering() {
    let rig = Rig::new(Options::default()).await;
    let body = create_body("dyn-a", "audited setup");
    let created = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("audit-create"),
            &[],
        )
        .await;
    assert_eq!(created.status, 200, "create must succeed");
    let wrong = format!("Bearer {WRONG_TOKEN_FOR_LOG_TEST}");
    let denied = rig
        .call(
            Method::GET,
            &rig.admin,
            "/v1/streams",
            None,
            Some(&wrong),
            None,
            &[],
        )
        .await;
    assert_eq!(denied.status, 401, "the wrong credential is refused");
    let failed = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some("{\"name\":\"dyn-b\",\"reason\":\"bad\",\"extra\":true}"),
            Some("audit-failed"),
            &[],
        )
        .await;
    assert_eq!(failed.status, 400, "a bad body is refused");
    let reason = serde_json::json!({"reason": "audited change", "limits": {"rpm": 20}}).to_string();
    let update = rig
        .stream_call(
            Method::PUT,
            "/v1/streams/dyn-a",
            Some(&reason),
            Some("audit-update"),
            &[],
        )
        .await;
    assert_eq!(update.status, 200, "update must succeed");
    let delete = serde_json::json!({"reason": "audited delete"}).to_string();
    let deleted = rig
        .stream_call(
            Method::DELETE,
            "/v1/streams/dyn-a",
            Some(&delete),
            Some("audit-delete"),
            &[],
        )
        .await;
    assert_eq!(deleted.status, 200, "delete must succeed");

    let audit_path = rig.audit_path();
    assert!(
        rig.head_path().exists(),
        "the audit head sidecar must exist"
    );
    let count = reiki::stream_admin::verify_audit(&audit_path).unwrap();
    assert!(
        count >= 5,
        "mutations, a denial and a failure are all appended"
    );
    let records = reiki::stream_admin::audit_head(&audit_path, count).unwrap();
    assert!(
        records
            .iter()
            .any(|record| record.action == "create" && record.outcome == "success")
    );
    assert!(
        records
            .iter()
            .any(|record| record.action == "update" && record.outcome == "success")
    );
    assert!(
        records
            .iter()
            .any(|record| record.action == "delete" && record.outcome == "success")
    );
    assert!(records.iter().any(|record| record.outcome == "denied"));
    assert!(records.iter().any(|record| record.outcome == "failed"));

    let original = std::fs::read_to_string(&audit_path).unwrap();
    let mut tail_deleted = audit_lines(&audit_path);
    tail_deleted.pop();
    std::fs::write(&audit_path, tail_deleted.join("\n") + "\n").unwrap();
    assert!(
        reiki::stream_admin::verify_audit(&audit_path).is_err(),
        "deleting the tail must break verification"
    );

    std::fs::write(&audit_path, &original).unwrap();
    let mut edited = audit_lines(&audit_path);
    let mut first: Value = serde_json::from_str(&edited[0]).unwrap();
    first["reason"] = serde_json::json!("tampered offline");
    edited[0] = first.to_string();
    std::fs::write(&audit_path, edited.join("\n") + "\n").unwrap();
    assert!(
        reiki::stream_admin::verify_audit(&audit_path).is_err(),
        "editing a record must break verification"
    );
}

#[tokio::test]
async fn store_failures_fail_closed_without_changing_memory() {
    let audit_blocked = Rig::new(Options::default()).await;
    std::fs::create_dir(audit_blocked.audit_path()).unwrap();
    let body = create_body("dyn-a", "audit unavailable");
    let reply = audit_blocked
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("store-audit"),
            &[],
        )
        .await;
    assert_eq!(
        reply.status, 503,
        "an unwritable audit log refuses the mutation"
    );
    assert_eq!(
        reply.json()["error"]["message"].as_str().unwrap(),
        STORE_MESSAGE,
        "the refusal is fixed"
    );
    assert!(!audit_blocked.state_path().exists(), "no state was written");
    let list = audit_blocked
        .stream_call(Method::GET, "/v1/streams", None, None, &[])
        .await
        .json();
    assert_eq!(
        list["streams"].as_array().unwrap().len(),
        0,
        "memory is unchanged"
    );

    let rig = Rig::new(Options::default()).await;
    let created = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("store-state-create"),
            &[],
        )
        .await;
    assert_eq!(created.status, 200, "an initial stream exists");
    std::fs::remove_file(rig.state_path()).unwrap();
    std::fs::create_dir(rig.state_path()).unwrap();
    let update =
        serde_json::json!({"reason": "state unavailable", "limits": {"rpm": 20}}).to_string();
    let reply = rig
        .stream_call(
            Method::PUT,
            "/v1/streams/dyn-a",
            Some(&update),
            Some("store-state-update"),
            &[],
        )
        .await;
    assert_eq!(
        reply.status, 503,
        "an unwritable state file refuses the mutation"
    );
    assert_eq!(
        reply.json()["error"]["message"].as_str().unwrap(),
        STORE_MESSAGE,
        "the refusal is fixed"
    );
    assert_no_credential(&reply, &[STREAM_ADMIN_TOKEN]);
    let shown = rig
        .stream_call(Method::GET, "/v1/streams/dyn-a", None, None, &[])
        .await
        .json();
    assert_eq!(
        shown["limits"]["rpm"], 10,
        "the rejected update changed nothing in memory"
    );
    let records = reiki::stream_admin::audit_head(&rig.audit_path(), 16).unwrap();
    assert!(
        records.iter().any(|record| record.outcome == "failed"),
        "a failed state write is audited"
    );
}

#[tokio::test]
async fn a_dynamic_token_works_then_expires_and_revocation_is_immediate() {
    let rig = Rig::new(Options::default()).await;
    let short = serde_json::json!({
        "name": "short-lived", "key": "zen-a", "reason": "short client", "ttl": 3
    })
    .to_string();
    let created = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&short),
            Some("expiry-create"),
            &[],
        )
        .await;
    assert_eq!(created.status, 200, "a short-lived token may be created");
    let token = created_token(&created);
    assert_eq!(
        rig.send_relay(&token).await,
        200,
        "the token works before expiry"
    );
    tokio::time::sleep(Duration::from_millis(3300)).await;
    assert_eq!(
        rig.send_relay(&token).await,
        401,
        "the token is refused after expiry"
    );
    assert_eq!(
        rig.send_relay(STATIC_TOKEN).await,
        200,
        "static streams remain usable"
    );

    let long = create_body("revoked", "long-lived client");
    let created = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&long),
            Some("revoke-create"),
            &[],
        )
        .await;
    assert_eq!(created.status, 200, "the long-lived stream was created");
    let token = created_token(&created);
    let token_id = created.json()["token_id"].as_str().unwrap().to_owned();
    assert_eq!(rig.send_relay(&token).await, 200, "the second token works");
    let revoke = serde_json::json!({"reason": "client revoked"}).to_string();
    let reply = rig
        .stream_call(
            Method::POST,
            &format!("/v1/streams/revoked/tokens/{token_id}/revoke"),
            Some(&revoke),
            Some("revoke-token"),
            &[],
        )
        .await;
    assert_eq!(reply.status, 200, "the token was revoked");
    assert_eq!(
        rig.send_relay(&token).await,
        401,
        "a revoked token stops working at once"
    );
}

#[tokio::test]
async fn refused_credentials_are_absent_from_the_service_log() {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("reiki-stream-admin-log-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log_path = dir.join("service.log");
    let log_config = reiki::config::LogConfig {
        file: Some(log_path.display().to_string()),
        stderr: false,
        ..Default::default()
    };
    let _logging = reiki::logging::init(&log_config, true).unwrap();
    tracing::info!("stream-admin-log-sink-marker");

    let rig = Rig::new(Options::default()).await;
    let wrong = format!("Bearer {WRONG_TOKEN_FOR_LOG_TEST}");
    let reply = rig
        .call(
            Method::GET,
            &rig.admin,
            "/v1/streams",
            None,
            Some(&wrong),
            None,
            &[],
        )
        .await;
    assert_eq!(reply.status, 401, "the credential is refused");
    assert_no_credential(&reply, &[WRONG_TOKEN_FOR_LOG_TEST, STREAM_ADMIN_TOKEN]);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("stream-admin-log-sink-marker"),
        "the test log sink is active"
    );
    assert!(
        !log.contains(WRONG_TOKEN_FOR_LOG_TEST),
        "a refused token must not be logged"
    );
    assert!(
        !log.contains(STREAM_ADMIN_TOKEN),
        "the configured token must not be logged"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "top")]
#[tokio::test]
async fn top_reads_fleet_stream_counts_from_a_real_health_document() {
    let mut options = Options::default();
    #[cfg(feature = "peers")]
    {
        options.peers = true;
    }
    let rig = Rig::new(options).await;
    let body = create_body("dyn-a", "health setup");
    let created = rig
        .stream_call(
            Method::POST,
            "/v1/streams/create",
            Some(&body),
            Some("top-create"),
            &[],
        )
        .await;
    assert_eq!(created.status, 200, "a dynamic stream was created");
    let peer_url = rig.peer.as_ref().unwrap();
    let auth = format!("Bearer {PEER_TOKEN}");
    let health = rig
        .call(
            Method::GET,
            peer_url,
            "/v1/health",
            None,
            Some(&auth),
            None,
            &[],
        )
        .await;
    assert_eq!(health.status, 200, "the peer health endpoint answers");
    assert_eq!(
        health.json()["streams"]["static_streams"],
        1,
        "one static stream is published"
    );
    assert_eq!(
        health.json()["streams"]["dynamic_streams"],
        1,
        "one dynamic stream is published"
    );

    let token_file = rig.dir.join("peer.token");
    common::write_key(&token_file, PEER_TOKEN);
    let reader_config = rig.dir.join("top.toml");
    std::fs::write(
        &reader_config,
        format!(
            "[peers]\nenabled = true\n[peers.local]\nurl = \"{peer_url}\"\ntoken_file = \"{}\"\n",
            token_file.display()
        ),
    )
    .unwrap();
    let path = reader_config.clone();
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_reiki"))
            .arg("--config")
            .arg(&path)
            .args(["top", "--once", "--full"])
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "top --once reads the peer: {err}");
    assert!(
        out.contains("Streams: 1 static, 1 dynamic"),
        "top shows fleet stream counts: {out}"
    );
}
