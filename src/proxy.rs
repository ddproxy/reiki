use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use futures_core::Stream;

use crate::auth::{constant_time_eq, extract_token, sha256_hex, strip_query_key};
use crate::chain::{Chain, KeyState, Policy, SavedHealth};
use crate::classify::{classify, FailureClass, Outcome};
use crate::config::Config;
use crate::metrics::{FailureKind, Metrics, StatusClass};
use crate::plugins::PluginChain;
use crate::rewrite::rewrite_body;

const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;
const MAX_ERROR_BODY: usize = 64 * 1024;
const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
const HEALTH_PATH: &str = "/__reiki/healthz";
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Fixed-window request counter for one stream. A window is a minute long; the
/// first request starts it. Counters live in the snapshot, so a reload resets them.
struct RateLimiter {
    limit: u32,
    window_start: Instant,
    used: u32,
}

impl RateLimiter {
    fn new(limit: u32, now: Instant) -> Self {
        Self {
            limit,
            window_start: now,
            used: 0,
        }
    }

    /// Counts one request against the window. Returns the wait until the window
    /// resets when the request is over the limit; the counter is not advanced then.
    fn check(&mut self, now: Instant) -> Result<(), Duration> {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed >= RATE_LIMIT_WINDOW {
            self.window_start = now;
            self.used = 0;
        }
        if self.used >= self.limit {
            return Err(
                RATE_LIMIT_WINDOW.saturating_sub(now.saturating_duration_since(self.window_start))
            );
        }
        self.used += 1;
        Ok(())
    }
}

pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug)]
pub enum LoadError {
    Read { key: String, source: std::io::Error },
    Empty(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { key, source } => write!(f, "cannot read key file for `{key}`: {source}"),
            Self::Empty(key) => write!(f, "key file for `{key}` is empty"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Empty(_) => None,
        }
    }
}

pub fn load_keys(config: &Config) -> Result<HashMap<String, Secret>, LoadError> {
    let mut keys = HashMap::new();
    for (name, key) in &config.keys {
        let raw = std::fs::read_to_string(&key.file).map_err(|source| LoadError::Read {
            key: name.clone(),
            source,
        })?;
        let value = raw.trim();
        if value.is_empty() {
            return Err(LoadError::Empty(name.clone()));
        }
        keys.insert(name.clone(), Secret::new(value));
    }
    Ok(keys)
}

pub(crate) struct Snapshot {
    pub(crate) config: Config,
    pub(crate) keys: HashMap<String, Secret>,
    pub(crate) chains: HashMap<String, Mutex<Chain>>,
    limiters: HashMap<String, Mutex<RateLimiter>>,
}

impl Snapshot {
    fn new(config: Config, keys: HashMap<String, Secret>) -> Self {
        let chains = config
            .streams
            .iter()
            .map(|(name, stream)| {
                (
                    name.clone(),
                    Mutex::new(Chain::new(stream.chain.clone(), stream.strategy.into())),
                )
            })
            .collect();
        let now = Instant::now();
        let limiters = config
            .streams
            .iter()
            .filter_map(|(name, stream)| {
                stream
                    .rate_limit_per_minute
                    .map(|limit| (name.clone(), Mutex::new(RateLimiter::new(limit, now))))
            })
            .collect();
        Self {
            config,
            keys,
            chains,
            limiters,
        }
    }

    /// Counts a request against the stream's rate limit, if it has one. Returns
    /// the wait until the window resets when the stream is over the limit.
    fn rate_limit_retry_after(&self, stream: &str, now: Instant) -> Option<Duration> {
        let mut limiter = self.limiters.get(stream)?.lock().ok()?;
        limiter.check(now).err()
    }

    fn authenticate(&self, token: &str) -> Option<&str> {
        let hash = sha256_hex(token);
        let mut found = None;
        // Check every stream so timing does not reveal which one matched.
        for (name, stream) in &self.config.streams {
            if constant_time_eq(&stream.token_sha256.to_ascii_lowercase(), &hash) {
                found = Some(name.as_str());
            }
        }
        found
    }

    fn pick(
        &self,
        health: &Mutex<KeyState>,
        stream: &str,
        upstream: &str,
        skip: &[String],
    ) -> Option<String> {
        let health = health.lock().unwrap_or_else(PoisonError::into_inner);
        let mut chain = self
            .chains
            .get(stream)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let keys = &self.config.keys;
        chain
            .pick_where(&health, Instant::now(), |k| {
                !skip.iter().any(|s| s == k) && keys.get(k).is_some_and(|c| c.upstream == upstream)
            })
            .map(str::to_owned)
    }

    fn soonest_recovery(&self, health: &Mutex<KeyState>, stream: &str) -> Option<Duration> {
        let health = health.lock().unwrap_or_else(PoisonError::into_inner);
        let chain = self
            .chains
            .get(stream)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        health.soonest_recovery(chain.keys(), Instant::now())
    }
}

pub struct App {
    snapshot: RwLock<Arc<Snapshot>>,
    health: Mutex<KeyState>,
    policy: Policy,
    client: reqwest::Client,
    header_timeout: Duration,
    metrics: Metrics,
    state_path: OnceLock<PathBuf>,
    /// Held across snapshot and write, so a stale snapshot cannot land after a newer one.
    persist_lock: Mutex<()>,
    /// Overrides the wall clock for tests; production reads the system clock.
    wall_clock: OnceLock<SystemTime>,
    plugins: Arc<PluginChain>,
}

impl App {
    pub fn new(
        config: Config,
        keys: HashMap<String, Secret>,
        policy: Policy,
        plugins: PluginChain,
    ) -> Arc<Self> {
        Self::with_plugins(config, keys, policy, RESPONSE_HEADER_TIMEOUT, plugins)
    }

    pub fn with_header_timeout(
        config: Config,
        keys: HashMap<String, Secret>,
        policy: Policy,
        header_timeout: Duration,
    ) -> Arc<Self> {
        Self::with_plugins(config, keys, policy, header_timeout, PluginChain::default())
    }

    pub fn with_plugins(
        config: Config,
        keys: HashMap<String, Secret>,
        policy: Policy,
        header_timeout: Duration,
        plugins: PluginChain,
    ) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            snapshot: RwLock::new(Arc::new(Snapshot::new(config, keys))),
            health: Mutex::new(KeyState::default()),
            policy,
            client,
            header_timeout,
            metrics: Metrics::new(),
            state_path: OnceLock::new(),
            persist_lock: Mutex::new(()),
            wall_clock: OnceLock::new(),
            plugins: Arc::new(plugins),
        })
    }

    pub(crate) fn snapshot(&self) -> Arc<Snapshot> {
        let guard = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }

    pub(crate) fn health(&self) -> &Mutex<KeyState> {
        &self.health
    }

    pub(crate) fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Point the app at the file it persists key health in. Set once at startup.
    pub fn set_state_path(&self, path: PathBuf) {
        let _ = self.state_path.set(path);
    }

    /// Fix the wall clock, so reset schedules can be tested against a known
    /// instant. Unset in production, where the system clock is read.
    pub fn set_wall_clock(&self, now: SystemTime) {
        let _ = self.wall_clock.set(now);
    }

    fn wall_now(&self) -> SystemTime {
        self.wall_clock
            .get()
            .copied()
            .unwrap_or_else(SystemTime::now)
    }

    /// Apply persisted key health. Missing or corrupt state is ignored.
    pub fn load_persisted_state(&self) {
        let Some(path) = self.state_path.get() else {
            return;
        };
        let config = self.snapshot();
        let saved = crate::state::load(path, &config.config);
        self.restore_state(saved);
    }

    fn restore_state(&self, saved: HashMap<String, SavedHealth>) {
        if saved.is_empty() {
            return;
        }
        if let Ok(mut health) = self.health.lock() {
            let now = Instant::now();
            for (name, saved_health) in saved {
                health.restore(&name, saved_health, now);
            }
        }
    }

    fn persist_state(&self) {
        let Some(path) = self.state_path.get() else {
            return;
        };
        let _guard = self
            .persist_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let snapshot = match self.health.lock() {
            Ok(health) => health.snapshot(Instant::now()),
            Err(_) => return,
        };
        crate::state::save(path, &snapshot);
    }

    /// Swap in a new config and key set. A request keeps the snapshot it started
    /// with. Plugins are built at startup and are not rebuilt here.
    pub fn reload(&self, config: Config, keys: HashMap<String, Secret>) {
        let next = Arc::new(Snapshot::new(config, keys));
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if current.config.listen != next.config.listen {
            tracing::warn!(
                from = %current.config.listen,
                to = %next.config.listen,
                "listen changed; restart reiki to change the listen address"
            );
        }
        let admin_listen = |snapshot: &Snapshot| {
            snapshot
                .config
                .admin
                .as_ref()
                .map(|admin| admin.listen.clone())
        };
        if admin_listen(&current) != admin_listen(&next) {
            tracing::warn!(
                from = ?admin_listen(&current),
                to = ?admin_listen(&next),
                "admin listener changed; restart reiki to apply it"
            );
        }
        if let Ok(mut health) = self.health.lock() {
            health.retain_keys(|name| next.keys.contains_key(name));
            for (name, secret) in &next.keys {
                let unchanged = current.keys.get(name).is_some_and(|old| old.0 == secret.0);
                if !unchanged {
                    health.enable(name);
                }
            }
        }
        *current = next;
    }

    /// The SIGHUP action. Live health is authoritative once running, so the state
    /// file is deliberately not replayed here: it would revive the disabled flag
    /// of a key whose secret was just rotated.
    pub fn reload_on_hangup(&self, path: &Path) {
        self.reload_from_file(path);
    }

    /// Re-read the config and key files, keeping the current snapshot on any error.
    pub fn reload_from_file(&self, path: &Path) {
        let loaded = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))
            .and_then(|text| Config::parse(&text).map_err(|e| format!("config: {e}")))
            .and_then(|config| {
                let keys = load_keys(&config).map_err(|e| e.to_string())?;
                Ok((config, keys))
            });
        match loaded {
            Ok((config, keys)) => {
                self.reload(config, keys);
                tracing::info!("reloaded config and keys");
            }
            Err(e) => tracing::warn!(error = %e, "reload failed; keeping the current config"),
        }
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

fn json_error(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = format!(
        "{{\"error\":{{\"type\":\"{kind}\",\"message\":\"{}\"}}}}",
        json_escape(message)
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Escapes text for use inside a JSON string literal.
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

fn rate_limited_response(wait: Duration) -> Response {
    let mut response = json_error(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "stream request rate limit exceeded",
    );
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    if let Ok(value) = HeaderValue::from_str(&secs.max(1).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn skip_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "accept-encoding"
    )
}

fn upstream_headers(inbound: &HeaderMap, auth_header: &str, auth_value: &str) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        if !skip_header(name) {
            out.append(name.clone(), value.clone());
        }
    }
    // Compressed error bodies cannot be classified, so ask for plain responses.
    out.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    if let (Ok(name), Ok(mut value)) = (
        HeaderName::from_bytes(auth_header.as_bytes()),
        HeaderValue::from_str(auth_value),
    ) {
        value.set_sensitive(true);
        out.insert(name, value);
    }
    out
}

fn split_upstream(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.strip_prefix('/')?;
    let (name, rest) = match trimmed.find('/') {
        Some(i) => (&trimmed[..i], &trimmed[i..]),
        None => (trimmed, ""),
    };
    if name.is_empty() {
        None
    } else {
        Some((name, rest))
    }
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let (mut parts, body) = req.into_parts();
    if parts.uri.path() == HEALTH_PATH && parts.method == Method::GET {
        return Response::new(Body::from("ok"));
    }

    // Take one snapshot for the whole request, so a reload cannot change what is
    // already in flight.
    let snapshot = app.snapshot();

    let Some(token) = extract_token(&parts.headers, parts.uri.query()) else {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized", "missing token");
    };
    let Some(stream) = snapshot.authenticate(&token) else {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized", "unknown token");
    };
    let stream = stream.to_owned();

    // A stream's own rate limit is checked before any key is picked, so a
    // rejected request never reaches the upstream or touches key health.
    if let Some(wait) = snapshot.rate_limit_retry_after(&stream, Instant::now()) {
        tracing::warn!(stream = %stream, "stream rate limit exceeded");
        app.metrics.record_rate_limit_rejection(&stream);
        return rate_limited_response(wait);
    }

    let Some((upstream_name, rest)) = split_upstream(parts.uri.path()) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no upstream in path");
    };
    let Some(upstream) = snapshot.config.upstream.get(upstream_name) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "unknown upstream");
    };

    let Ok(body) = to_bytes(body, MAX_REQUEST_BODY).await else {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "request body too large",
        );
    };
    let mut body = body;

    // Run request hooks once, before the failover loop, so every attempt sees
    // the same transformed request.
    let mut plugin_states = app.plugins.states();
    if let Err(blocked) = app.plugins.on_request(
        &mut plugin_states,
        &stream,
        upstream_name,
        &parts.method,
        &mut parts.headers,
        &mut body,
    ) {
        return json_error(blocked.status, "blocked", &blocked.message);
    }

    // Rewrite once, before the failover loop, so every key replay sends the same
    // bytes. reqwest sets content-length from the rewritten body on each send.
    let body = rewrite_body(body, &upstream.model_aliases, &upstream.drop_params);

    let response = forward(
        &app,
        Forward {
            snapshot: &snapshot,
            stream: &stream,
            upstream_name,
            upstream,
            parts: &parts,
            rest,
            plugin_states,
        },
        body,
    )
    .await;
    app.metrics.record_request(
        &stream,
        upstream_name,
        StatusClass::from_status(response.status().as_u16()),
    );
    response
}

/// Everything one in-flight request needs, kept together so `forward` stays
/// within the argument limit.
struct Forward<'a> {
    snapshot: &'a Snapshot,
    stream: &'a str,
    upstream_name: &'a str,
    upstream: &'a crate::config::Upstream,
    parts: &'a Parts,
    rest: &'a str,
    plugin_states: Vec<Box<dyn Any + Send>>,
}

async fn forward(app: &App, fwd: Forward<'_>, body: Bytes) -> Response {
    let Forward {
        snapshot,
        stream,
        upstream_name,
        upstream,
        parts,
        rest,
        mut plugin_states,
    } = fwd;

    let mut url = format!("{}{}", upstream.base_url.trim_end_matches('/'), rest);
    if let Some(query) = parts.uri.query().and_then(strip_query_key) {
        url.push('?');
        url.push_str(&query);
    }

    // Enough attempts to walk the chain once, plus one retry of a transient failure.
    let max_attempts = snapshot
        .chains
        .get(stream)
        .map_or(1, |_| snapshot.config.streams[stream].chain.len() + 1);

    let mut skip: Vec<String> = Vec::new();
    let mut last_refusal: Option<(StatusCode, HeaderMap, Bytes)> = None;
    let mut pending_failover: Option<(String, FailureKind)> = None;
    for _ in 0..max_attempts {
        let Some(key_name) = snapshot.pick(&app.health, stream, upstream_name, &skip) else {
            break;
        };
        // Only count a failover once the next key is actually in hand, so a dead
        // end does not inflate the counter.
        if let Some((failed_key, kind)) = pending_failover.take() {
            app.metrics.record_failover(stream, &failed_key, kind);
        }
        let Some(secret) = snapshot.keys.get(&key_name) else {
            break;
        };
        let auth_value = upstream.auth_format.replace("{key}", &secret.0);
        let headers = upstream_headers(&parts.headers, &upstream.auth_header, &auth_value);

        let sent = tokio::time::timeout(
            app.header_timeout,
            app.client
                .request(parts.method.clone(), &url)
                .headers(headers)
                .body(body.clone())
                .send(),
        )
        .await;

        let response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(_)) | Err(_) => {
                record(
                    app,
                    snapshot,
                    stream,
                    &key_name,
                    FailureClass::Transient,
                    None,
                );
                pending_failover = Some((key_name, FailureKind::Transient));
                continue;
            }
        };

        let status = response.status();
        tracing::info!(
            stream = %stream,
            key = %key_name,
            method = %parts.method,
            path = %parts.uri.path(),
            status = status.as_u16(),
            "forwarded"
        );
        if status.is_success() || status.is_redirection() {
            let had_failures = if let Ok(mut health) = app.health.lock() {
                let had = health.consecutive_failures(&key_name) > 0;
                health.record_success(&key_name);
                had
            } else {
                false
            };
            // Only write when this success changed what is on disk.
            if had_failures {
                app.persist_state();
            }
            return stream_response(app, plugin_states, response);
        }

        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let response_headers = response.headers().clone();
        let snippet = read_limited(response).await;
        let text = String::from_utf8_lossy(&snippet);

        match classify(status.as_u16(), retry_after.as_deref(), &text) {
            Outcome::Success | Outcome::PassThrough => {
                return finish_buffered(
                    &app.plugins,
                    &mut plugin_states,
                    status,
                    &response_headers,
                    snippet,
                );
            }
            Outcome::KeyFailure(FailureClass::AccessDisabled) => {
                // "Model access is disabled" can depend on the endpoint or model of this one
                // request (a client may be probing /responses before falling back to
                // /chat/completions), so it must not put the whole key on cooldown. Try the
                // next key; if none helps, hand the upstream's own answer back to the caller.
                tracing::debug!(status = status.as_u16(), body = %text.chars().take(160).collect::<String>(), "upstream refused this request");
                tracing::warn!(stream = %stream, key = %key_name, status = status.as_u16(), "key refused this request, trying the next key");
                skip.push(key_name.clone());
                pending_failover = Some((key_name, FailureKind::AccessDisabled));
                last_refusal = Some((status, response_headers, snippet));
            }
            Outcome::KeyFailure(class) => {
                tracing::debug!(status = status.as_u16(), body = %text.chars().take(160).collect::<String>(), "upstream error");
                record(
                    app,
                    snapshot,
                    stream,
                    &key_name,
                    class,
                    Some(status.as_u16()),
                );
                pending_failover = Some((key_name, FailureKind::from(class)));
            }
        }
    }

    if let Some((status, headers, body)) = last_refusal {
        return finish_buffered(&app.plugins, &mut plugin_states, status, &headers, body);
    }
    exhausted(snapshot, &app.health, stream)
}

fn record(
    app: &App,
    snapshot: &Snapshot,
    stream: &str,
    key: &str,
    class: FailureClass,
    status: Option<u16>,
) {
    let quota_cooldown = quota_cooldown(snapshot, key, class, app.wall_now());
    if let Ok(mut health) = app.health.lock() {
        let cooled =
            health.record_failure_with(key, class, &app.policy, Instant::now(), quota_cooldown);
        if cooled {
            app.metrics.record_cooldown(key, FailureKind::from(class));
        }
        tracing::warn!(stream, key, ?class, ?status, cooled, "upstream key failed");
    }
    app.persist_state();
}

/// The cooldown for a quota failure when the key names a reset schedule; `None`
/// otherwise, so the fixed policy window applies.
fn quota_cooldown(
    snapshot: &Snapshot,
    key: &str,
    class: FailureClass,
    now: SystemTime,
) -> Option<Duration> {
    if class != FailureClass::QuotaExhausted {
        return None;
    }
    snapshot.config.quota_reset(key)?.next_after(now)
}

fn exhausted(snapshot: &Snapshot, health: &Mutex<KeyState>, stream: &str) -> Response {
    let mut response = json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "reiki_exhausted",
        "every key in this stream's chain is unavailable",
    );
    if let Some(wait) = snapshot.soonest_recovery(health, stream) {
        let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        if let Ok(value) = HeaderValue::from_str(&secs.max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

async fn read_limited(mut response: reqwest::Response) -> Bytes {
    let mut collected = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_ERROR_BODY.saturating_sub(collected.len());
        collected.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if collected.len() >= MAX_ERROR_BODY {
            break;
        }
    }
    Bytes::from(collected)
}

fn copy_response_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        if !skip_header(name) {
            to.append(name.clone(), value.clone());
        }
    }
}

fn stream_response(
    app: &App,
    plugin_states: Vec<Box<dyn Any + Send>>,
    response: reqwest::Response,
) -> Response {
    let mut status = response.status();
    let mut headers = HeaderMap::new();
    copy_response_headers(response.headers(), &mut headers);
    // Pass the length through when the upstream gave one, so non-chunked bodies stay non-chunked.
    if let Some(length) = response.headers().get(header::CONTENT_LENGTH) {
        headers.insert(header::CONTENT_LENGTH, length.clone());
    }
    let body = if app.plugins.is_empty() {
        Body::from_stream(response.bytes_stream())
    } else {
        let mut states = plugin_states;
        // A plugin that changes the body length must drop content-length here.
        app.plugins
            .on_response_head(&mut states, &mut status, &mut headers);
        Body::from_stream(PluginBody {
            chain: Arc::clone(&app.plugins),
            states,
            inner: Box::pin(response.bytes_stream()),
            done: false,
        })
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// Runs the response hooks over a body that arrived in one piece.
fn finish_buffered(
    chain: &PluginChain,
    states: &mut [Box<dyn Any + Send>],
    status: StatusCode,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if chain.is_empty() {
        return buffered_response(status, headers, body);
    }
    let mut status = status;
    let mut headers = headers.clone();
    chain.on_response_head(states, &mut status, &mut headers);
    let mut body = body;
    chain.on_response_chunk(states, &mut body, true);
    buffered_response(status, &headers, body)
}

/// Streams the upstream body through the plugin chain a chunk at a time.
///
/// Each chunk is released as soon as its plugins have seen it, so a placeholder
/// split across chunks only delays the chunks it spans. The final empty poll
/// gives plugins the `is_last` call to release anything they held back.
struct PluginBody {
    chain: Arc<PluginChain>,
    states: Vec<Box<dyn Any + Send>>,
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    done: bool,
}

impl Stream for PluginBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;

        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    let mut tail = Bytes::new();
                    this.chain
                        .on_response_chunk(&mut this.states, &mut tail, true);
                    return if tail.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(tail)))
                    };
                }
                Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(std::io::Error::other(error))));
                }
                Poll::Ready(Some(Ok(mut chunk))) => {
                    this.chain
                        .on_response_chunk(&mut this.states, &mut chunk, false);
                    if chunk.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
            }
        }
    }
}

fn buffered_response(status: StatusCode, headers: &HeaderMap, body: Bytes) -> Response {
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = status;
    copy_response_headers(headers, out.headers_mut());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_produces_text_a_json_parser_gives_back_unchanged() {
        for text in [
            "plain",
            "quote \" and backslash \\ and slash /",
            "line\nbreak\r\ttab",
            "control \u{1} \u{1f} end",
            "unicode é — 日本",
            "",
        ] {
            let wrapped = format!("\"{}\"", json_escape(text));
            let parsed: String = serde_json::from_str(&wrapped).unwrap();
            assert_eq!(parsed, text, "{wrapped}");
        }
    }

    #[test]
    fn splits_the_upstream_name_off_the_path() {
        assert_eq!(
            split_upstream("/zen/chat/completions"),
            Some(("zen", "/chat/completions"))
        );
        assert_eq!(split_upstream("/zen"), Some(("zen", "")));
        assert_eq!(split_upstream("/"), None);
        assert_eq!(split_upstream("//x"), None);
    }

    #[test]
    fn rate_limiter_allows_the_limit_then_reports_a_reset_wait() {
        let now = Instant::now();
        let mut limiter = RateLimiter::new(2, now);
        assert_eq!(limiter.check(now), Ok(()));
        assert_eq!(limiter.check(now), Ok(()));
        let wait = limiter.check(now).unwrap_err();
        assert!(wait > Duration::ZERO && wait <= RATE_LIMIT_WINDOW);

        // The wait shrinks as the window advances, then the window resets.
        let later = limiter.check(now + Duration::from_secs(45)).unwrap_err();
        assert!(later < wait, "{later:?} was not shorter than {wait:?}");
        assert_eq!(limiter.check(now + RATE_LIMIT_WINDOW), Ok(()));
    }

    #[test]
    fn a_rejected_request_does_not_advance_the_used_count() {
        let now = Instant::now();
        let mut limiter = RateLimiter::new(1, now);
        assert_eq!(limiter.check(now), Ok(()));
        for _ in 0..5 {
            assert!(limiter.check(now).is_err());
        }
        assert_eq!(limiter.check(now + RATE_LIMIT_WINDOW), Ok(()));
    }

    #[test]
    fn secrets_never_print() {
        assert_eq!(format!("{:?}", Secret::new("sk-123")), "Secret(<redacted>)");
    }

    #[test]
    fn caller_credentials_are_not_forwarded_and_the_upstream_key_is_set() {
        let mut inbound = HeaderMap::new();
        inbound.insert(
            "authorization",
            HeaderValue::from_static("Bearer caller-token"),
        );
        inbound.insert("x-api-key", HeaderValue::from_static("caller-token"));
        inbound.insert("x-goog-api-key", HeaderValue::from_static("caller-token"));
        inbound.insert("user-agent", HeaderValue::from_static("opencode/1.0"));
        inbound.insert("x-custom", HeaderValue::from_static("keep"));
        inbound.insert("connection", HeaderValue::from_static("close"));
        inbound.insert("host", HeaderValue::from_static("127.0.0.1:4097"));
        let out = upstream_headers(&inbound, "authorization", "Bearer upstream-key");
        assert_eq!(out["authorization"], "Bearer upstream-key");
        assert_eq!(out["user-agent"], "opencode/1.0");
        assert_eq!(out["x-custom"], "keep");
        assert!(out.get("x-api-key").is_none());
        assert!(out.get("x-goog-api-key").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("host").is_none());
        assert!(!out
            .values()
            .any(|v| v.to_str().is_ok_and(|s| s.contains("caller-token"))));
    }
    #[test]
    fn a_poisoned_lock_does_not_turn_every_pick_into_a_miss() {
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
             [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n",
            crate::auth::sha256_hex("tok")
        );
        let config = Config::parse(&text).unwrap();
        let keys = HashMap::from([("a".to_owned(), Secret::new("up-a"))]);
        let snapshot = Snapshot::new(config, keys);
        let health = Arc::new(Mutex::new(KeyState::default()));

        let poisoner = Arc::clone(&health);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the health lock");
        })
        .join();
        assert!(health.is_poisoned());

        assert_eq!(
            snapshot.pick(&health, "s", "zen", &[]),
            Some("a".to_owned())
        );
        assert!(snapshot.soonest_recovery(&health, "s").is_some());
    }
}
