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
use crate::forward::Forwarder;
use crate::plugins::{PluginChain, RequestMeta};
use crate::rewrite::rewrite_body;
use crate::upstream::{Call, Faithful, Standard, Transport, UpstreamResponse};
use reiki_plugin::{Event, FailureKind};

const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;
const FORWARDER_FAILED_BODY: &[u8] =
    b"{\"error\":{\"type\":\"bad_gateway\",\"message\":\"the upstream could not be reached\"}}";
const MAX_ERROR_BODY: usize = 64 * 1024;
const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
/// Stands in for the stream name when a request needs none.
const PASSTHROUGH_STREAM: &str = "passthrough";
/// Stands in for the key name on a request that used none of Reiki's keys.
const PASSTHROUGH_KEY: &str = "-";
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
    standard: Standard,
    faithful: Faithful,
    forwarder: Forwarder,
    header_timeout: Duration,
    state_path: OnceLock<PathBuf>,
    /// Held across snapshot and write, so a stale snapshot cannot land after a newer one.
    persist_lock: Mutex<()>,
    /// Overrides the wall clock for tests; production reads the system clock.
    wall_clock: OnceLock<SystemTime>,
    /// When the interception CA expires, as Unix seconds; unset without interception.
    ca_not_after: OnceLock<i64>,
    plugins: Arc<PluginChain>,
    /// The plugin tables the running chain was built from, which a reload cannot change.
    plugin_tables: std::collections::BTreeMap<String, toml::Table>,
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
        let plugin_tables = config.plugins.clone();
        Arc::new(Self {
            snapshot: RwLock::new(Arc::new(Snapshot::new(config, keys))),
            health: Mutex::new(KeyState::default()),
            policy,
            standard: Standard::new(),
            faithful: Faithful::new(),
            forwarder: Forwarder,
            header_timeout,
            state_path: OnceLock::new(),
            persist_lock: Mutex::new(()),
            wall_clock: OnceLock::new(),
            ca_not_after: OnceLock::new(),
            plugins: Arc::new(plugins),
            plugin_tables,
        })
    }

    /// Forward wins when configured; otherwise faithful keeps the client request, standard rebuilds it.
    fn transport(&self, faithful: bool, upstream: &crate::config::Upstream) -> &dyn Transport {
        if upstream.forward.is_some() {
            &self.forwarder
        } else if faithful {
            &self.faithful
        } else {
            &self.standard
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<Snapshot> {
        let guard = self.snapshot.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }

    pub(crate) fn plugins(&self) -> &PluginChain {
        &self.plugins
    }

    pub(crate) fn health(&self) -> &Mutex<KeyState> {
        &self.health
    }

    /// Point the app at the file it persists key health in. Set once at startup.
    pub fn set_state_path(&self, path: PathBuf) {
        let _ = self.state_path.set(path);
    }

    /// Fix the wall clock, so reset schedules can be tested against a known
    /// instant. Unset in production, where the system clock is read.
    pub fn set_ca_not_after(&self, unix_seconds: i64) {
        let _ = self.ca_not_after.set(unix_seconds);
    }

    pub(crate) fn ca_not_after(&self) -> Option<i64> {
        self.ca_not_after.get().copied()
    }

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
        if self.plugin_tables != next.config.plugins {
            tracing::warn!("plugin settings changed; restart reiki to apply them");
        }
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
                let dry = crate::plugins::with_dry_run(&config.plugins);
                crate::plugins::PluginChain::build(&dry, &crate::plugins::builtin())
                    .map_err(|e| format!("config: {e}"))?;
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

pub(crate) fn upstream_headers(
    inbound: &HeaderMap,
    auth_header: &str,
    auth_value: &str,
    forward_accept_encoding: bool,
    send_keep_alive: bool,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    // The credential goes first, where most clients put it.
    let auth_name = HeaderName::from_bytes(auth_header.as_bytes()).ok();
    if let (Some(name), Ok(mut value)) = (&auth_name, HeaderValue::from_str(auth_value)) {
        value.set_sensitive(true);
        out.insert(name.clone(), value);
    }
    for (name, value) in inbound {
        let keep_encoding = forward_accept_encoding && name == header::ACCEPT_ENCODING;
        if auth_name.as_ref() == Some(name) || !(keep_encoding || !skip_header(name)) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    // Unless the client's own value is forwarded (the client sent none, or the upstream
    // is set to pass it on), ask for plain responses so error bodies can be classified.
    if !out.contains_key(header::ACCEPT_ENCODING) {
        out.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    if send_keep_alive {
        out.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
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
    let (parts, body) = req.into_parts();
    if parts.uri.path() == HEALTH_PATH && parts.method == Method::GET {
        return Response::new(Body::from("ok"));
    }

    // Take one snapshot for the whole request, so a reload cannot change what is
    // already in flight.
    let snapshot = app.snapshot();

    // The client's own credentials go upstream, so there is no Reiki token to check.
    let passthrough = split_upstream(parts.uri.path())
        .and_then(|(name, _)| snapshot.config.upstream.get(name))
        .is_some_and(|u| u.passthrough);
    let stream = if passthrough {
        PASSTHROUGH_STREAM.to_owned()
    } else {
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
            app.plugins
                .on_event(&Event::RateLimited { stream: &stream });
            return rate_limited_response(wait);
        }
        stream
    };

    let Some((upstream_name, rest)) = split_upstream(parts.uri.path()) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no upstream in path");
    };
    let Some(upstream) = snapshot.config.upstream.get(upstream_name) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "unknown upstream");
    };

    let (upstream_name, rest) = (upstream_name.to_owned(), rest.to_owned());
    dispatch(
        &app,
        &snapshot,
        &stream,
        &upstream_name,
        upstream,
        rest,
        parts,
        body,
        false,
    )
    .await
}

/// What `[tunnel.intercept]` maps one host to.
pub(crate) async fn handle_intercepted(
    app: &Arc<App>,
    host: &str,
    target: &crate::config::InterceptHost,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let named = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| h.trim().to_ascii_lowercase());
    if named.as_deref() != Some(host) && named.as_deref() != Some(&format!("{host}:443")) {
        tracing::warn!(%host, header = ?named, "intercepted request refused: Host header differs from the tunnel destination");
        return json_error(
            StatusCode::MISDIRECTED_REQUEST,
            "misdirected",
            "the Host header does not match the tunnel destination",
        );
    }

    let snapshot = app.snapshot();
    let Some(upstream) = snapshot.config.upstream.get(&target.upstream) else {
        return json_error(StatusCode::BAD_GATEWAY, "bad_gateway", "unknown upstream");
    };
    let path = parts.uri.path();
    let sent;
    let (upstream, rest) = if target.paths.is_empty() {
        let Some(rest) = path_under_base(&upstream.base_url, path) else {
            return outside_api(host, path);
        };
        (upstream, rest)
    } else {
        if !target.paths.iter().any(|prefix| has_prefix(path, prefix)) {
            return outside_api(host, path);
        }
        let Ok(url) = reqwest::Url::parse(&upstream.base_url) else {
            return json_error(StatusCode::BAD_GATEWAY, "bad_gateway", "bad upstream URL");
        };
        // The base URL only names the host and credential format; the path goes upstream as sent.
        sent = crate::config::Upstream {
            base_url: url.origin().ascii_serialization(),
            ..upstream.clone()
        };
        (&sent, path.to_owned())
    };
    let stream = match (&target.stream, upstream.passthrough) {
        (Some(name), false) => name.as_str(),
        _ => PASSTHROUGH_STREAM,
    };
    if !upstream.passthrough {
        if let Some(wait) = snapshot.rate_limit_retry_after(stream, Instant::now()) {
            app.plugins.on_event(&Event::RateLimited { stream });
            return rate_limited_response(wait);
        }
    }
    dispatch(
        app,
        &snapshot,
        stream,
        &target.upstream,
        upstream,
        rest,
        parts,
        body,
        true,
    )
    .await
}

fn outside_api(host: &str, path: &str) -> Response {
    tracing::warn!(%host, %path, "intercepted request refused: path is outside the intercepted API");
    json_error(
        StatusCode::FORBIDDEN,
        "forbidden",
        "the path is outside the intercepted API",
    )
}

/// True when `path` is `prefix` or lies below it, on whole segments.
fn has_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The part of `path` below the upstream's base path, or `None` when it is not under it.
fn path_under_base(base_url: &str, path: &str) -> Option<String> {
    let base = reqwest::Url::parse(base_url)
        .ok()?
        .path()
        .trim_end_matches('/')
        .to_owned();
    if base.is_empty() {
        return Some(path.to_owned());
    }
    match path.strip_prefix(base.as_str()) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => Some(rest.to_owned()),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    app: &App,
    snapshot: &Snapshot,
    stream: &str,
    upstream_name: &str,
    upstream: &crate::config::Upstream,
    rest: String,
    parts: Parts,
    body: Body,
    faithful: bool,
) -> Response {
    use tracing::Instrument;
    let id = reiki_plugin::new_request_id();
    let span = tracing::info_span!("req", id = %id);
    dispatch_inner(
        app,
        snapshot,
        stream,
        upstream_name,
        upstream,
        rest,
        parts,
        body,
        &id,
        faithful,
    )
    .instrument(span)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_inner(
    app: &App,
    snapshot: &Snapshot,
    stream: &str,
    upstream_name: &str,
    upstream: &crate::config::Upstream,
    rest: String,
    mut parts: Parts,
    body: Body,
    id: &str,
    faithful: bool,
) -> Response {
    let rest = rest.as_str();
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
    let meta = RequestMeta {
        id,
        stream,
        upstream: upstream_name,
        method: &parts.method,
        path: parts.uri.path(),
    };
    if let Err(blocked) =
        app.plugins
            .on_request(&mut plugin_states, &meta, &mut parts.headers, &mut body)
    {
        return json_error(blocked.status, "blocked", &blocked.message);
    }
    if body.len() > MAX_REQUEST_BODY {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "request body too large",
        );
    }

    // Rewrite once, before the failover loop, so every key replay sends the same
    // bytes. reqwest sets content-length from the rewritten body on each send.
    let rewritten = rewrite_body(body.clone(), &upstream.model_aliases, &upstream.drop_params);

    let mut used = upstream_name;
    let first = Forward {
        snapshot,
        stream,
        upstream_name,
        upstream,
        parts: &parts,
        rest,
        plugin_states,
        faithful,
    };
    let mut result = if upstream.passthrough {
        forward_passthrough(app, first, rewritten).await
    } else {
        forward(app, first, rewritten).await
    };

    // One hop only: when nothing is left to try here, hand the request to the
    // fallback upstream, re-mapped for it, instead of failing the client.
    if let (Err(spent), Some(fallback)) = (&mut result, &upstream.fallback) {
        if let Some(target) = snapshot
            .config
            .upstream
            .get(&fallback.upstream)
            .filter(|_| !spent.link_failed)
        {
            tracing::warn!(stream = %stream, from = upstream_name, to = %fallback.upstream, "falling back");
            let failed = spent
                .refusal
                .as_ref()
                .map_or(StatusCode::SERVICE_UNAVAILABLE, |r| r.1);
            app.plugins.on_event(&Event::RequestDone {
                stream,
                upstream: upstream_name,
                status: failed.as_u16(),
            });
            let body = rewrite_body(body, &fallback.model_aliases, &[]);
            let body = rewrite_body(body, &target.model_aliases, &target.drop_params);
            used = fallback.upstream.as_str();
            result = forward(
                app,
                Forward {
                    snapshot,
                    stream,
                    upstream_name: used,
                    upstream: target,
                    parts: &parts,
                    rest,
                    plugin_states: std::mem::take(&mut spent.plugin_states),
                    faithful,
                },
                body,
            )
            .await;
        }
    }
    let response = match result {
        Ok(response) => response,
        Err(spent) => match spent.refusal {
            Some((key, status, headers, body)) => {
                let mut states = spent.plugin_states;
                finish_buffered(&app.plugins, &mut states, &key, status, &headers, body)
            }
            None => exhausted(snapshot, &app.health, stream),
        },
    };
    app.plugins.on_event(&Event::RequestDone {
        stream,
        upstream: used,
        status: response.status().as_u16(),
    });
    response
}

/// A request whose chain ran dry; the plugin state lets a fallback continue it.
struct Spent {
    refusal: Option<(String, StatusCode, HeaderMap, Bytes)>,
    plugin_states: Vec<Box<dyn Any + Send>>,
    /// The forwarder link failed, which is final: no fallback upstream is tried.
    link_failed: bool,
}

fn forwarder_failed(key: String, plugin_states: Vec<Box<dyn Any + Send>>) -> Box<Spent> {
    Box::new(Spent {
        refusal: Some((
            key,
            StatusCode::BAD_GATEWAY,
            HeaderMap::new(),
            Bytes::from_static(FORWARDER_FAILED_BODY),
        )),
        plugin_states,
        link_failed: true,
    })
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
    faithful: bool,
}

async fn forward(app: &App, fwd: Forward<'_>, body: Bytes) -> Result<Response, Box<Spent>> {
    let Forward {
        snapshot,
        stream,
        upstream_name,
        upstream,
        parts,
        rest,
        mut plugin_states,
        faithful,
    } = fwd;

    let url = target_url(upstream, rest, parts);

    // Enough attempts to walk the chain once, plus one retry of a transient failure.
    let max_attempts = snapshot
        .chains
        .get(stream)
        .map_or(1, |_| snapshot.config.streams[stream].chain.len() + 1);

    let mut skip: Vec<String> = Vec::new();
    let mut last_refusal: Option<(String, StatusCode, HeaderMap, Bytes)> = None;
    let mut pending_failover: Option<(String, FailureKind)> = None;
    for _ in 0..max_attempts {
        let Some(key_name) = snapshot.pick(&app.health, stream, upstream_name, &skip) else {
            break;
        };
        // Only count a failover once the next key is actually in hand, so a dead
        // end does not inflate the counter.
        if let Some((failed_key, kind)) = pending_failover.take() {
            app.plugins.on_event(&Event::Failover {
                stream,
                key: &failed_key,
                kind,
            });
        }
        let Some(secret) = snapshot.keys.get(&key_name) else {
            break;
        };
        let auth_value = upstream.auth_format.replace("{key}", &secret.0);

        // For a forward upstream this bounds the whole exchange, not only response headers.
        let sent = tokio::time::timeout(app.header_timeout, async {
            app.transport(faithful, upstream)
                .send(Call {
                    parts,
                    url: &url,
                    credential: Some((&upstream.auth_header, &auth_value)),
                    body: body.clone(),
                    upstream,
                })
                .await
        })
        .await;

        let response = match sent {
            Ok(Ok(response)) => response,
            failed => {
                if upstream.forward.is_some() {
                    let reason = match &failed {
                        Ok(Err(err)) => err.to_string(),
                        _ => "timed out".to_owned(),
                    };
                    tracing::error!(error = %reason, "forwarder failed");
                    app.plugins
                        .on_attempt(&mut plugin_states, &key_name, None, &[]);
                    return Err(forwarder_failed(key_name, plugin_states));
                }
                app.plugins
                    .on_attempt(&mut plugin_states, &key_name, None, &[]);
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

        let status = response.status;
        tracing::info!(
            stream = %stream,
            key = %key_name,
            method = %parts.method,
            path = %parts.uri.path(),
            status = status.as_u16(),
            version = ?response.version,
            "forwarded"
        );
        if status.is_success() || status.is_redirection() {
            app.plugins
                .on_attempt(&mut plugin_states, &key_name, Some(status.as_u16()), &[]);
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
            return Ok(stream_response(
                app,
                plugin_states,
                &key_name,
                response,
                parts.method == Method::HEAD,
            ));
        }

        let retry_after = response
            .headers
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let response_headers = response.headers.clone();
        let snippet = read_limited(response).await;
        app.plugins.on_attempt(
            &mut plugin_states,
            &key_name,
            Some(status.as_u16()),
            &snippet,
        );
        let text = String::from_utf8_lossy(&snippet);

        match classify(status.as_u16(), retry_after.as_deref(), &text) {
            Outcome::Success | Outcome::PassThrough => {
                return Ok(finish_buffered(
                    &app.plugins,
                    &mut plugin_states,
                    &key_name,
                    status,
                    &response_headers,
                    snippet,
                ));
            }
            Outcome::KeyFailure(FailureClass::AccessDisabled) => {
                // "Model access is disabled" can depend on the endpoint or model of this one
                // request (a client may be probing /responses before falling back to
                // /chat/completions), so it must not put the whole key on cooldown. Try the
                // next key; if none helps, hand the upstream's own answer back to the caller.
                tracing::debug!(status = status.as_u16(), body = %text.chars().take(160).collect::<String>(), "upstream refused this request");
                tracing::warn!(stream = %stream, key = %key_name, status = status.as_u16(), "key refused this request, trying the next key");
                skip.push(key_name.clone());
                last_refusal = Some((key_name.clone(), status, response_headers, snippet));
                pending_failover = Some((key_name, FailureKind::AccessDisabled));
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

    Err(Box::new(Spent {
        refusal: last_refusal,
        plugin_states,
        link_failed: false,
    }))
}

/// Sends the request as the client made it, credentials included, and returns any answer.
async fn forward_passthrough(
    app: &App,
    fwd: Forward<'_>,
    body: Bytes,
) -> Result<Response, Box<Spent>> {
    let Forward {
        upstream,
        parts,
        rest,
        mut plugin_states,
        faithful,
        ..
    } = fwd;
    let url = target_url(upstream, rest, parts);
    // For a forward upstream this bounds the whole exchange, not only response headers.
    let sent = tokio::time::timeout(app.header_timeout, async {
        app.transport(faithful, upstream)
            .send(Call {
                parts,
                url: &url,
                credential: None,
                body,
                upstream,
            })
            .await
    })
    .await;
    match sent {
        Ok(Ok(response)) => {
            let status = response.status;
            tracing::info!(method = %parts.method, path = %parts.uri.path(), status = status.as_u16(), version = ?response.version, "passed through");
            app.plugins.on_attempt(
                &mut plugin_states,
                PASSTHROUGH_KEY,
                Some(status.as_u16()),
                &[],
            );
            Ok(stream_response(
                app,
                plugin_states,
                PASSTHROUGH_KEY,
                response,
                parts.method == Method::HEAD,
            ))
        }
        Ok(Err(err)) => {
            if upstream.forward.is_some() {
                tracing::error!(error = %err, "forwarder failed");
                app.plugins
                    .on_attempt(&mut plugin_states, PASSTHROUGH_KEY, None, &[]);
                return Ok(json_error(
                    StatusCode::BAD_GATEWAY,
                    "bad_gateway",
                    "the upstream could not be reached",
                ));
            }
            app.plugins
                .on_attempt(&mut plugin_states, PASSTHROUGH_KEY, None, &[]);
            tracing::warn!(%err, "passthrough request failed");
            Ok(json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                "the upstream could not be reached",
            ))
        }
        Err(_) => {
            if upstream.forward.is_some() {
                tracing::error!(error = "timed out", "forwarder failed");
                app.plugins
                    .on_attempt(&mut plugin_states, PASSTHROUGH_KEY, None, &[]);
                return Ok(json_error(
                    StatusCode::BAD_GATEWAY,
                    "bad_gateway",
                    "the upstream could not be reached",
                ));
            }
            app.plugins
                .on_attempt(&mut plugin_states, PASSTHROUGH_KEY, None, &[]);
            Ok(json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "gateway_timeout",
                "the upstream did not answer in time",
            ))
        }
    }
}

fn target_url(upstream: &crate::config::Upstream, rest: &str, parts: &Parts) -> String {
    let mut url = format!("{}{}", upstream.base_url.trim_end_matches('/'), rest);
    if let Some(query) = parts.uri.query().and_then(strip_query_key) {
        url.push('?');
        url.push_str(&query);
    }
    url
}

/// Every end-to-end header as the client sent it, credentials included.
pub(crate) fn passthrough_headers(
    inbound: &HeaderMap,
    forward_accept_encoding: bool,
    send_keep_alive: bool,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        let keep = matches!(
            name.as_str(),
            "authorization" | "x-api-key" | "x-goog-api-key"
        ) || (forward_accept_encoding && name == header::ACCEPT_ENCODING);
        if keep || !skip_header(name) {
            out.append(name.clone(), value.clone());
        }
    }
    if !out.contains_key(header::ACCEPT_ENCODING) {
        out.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    if send_keep_alive {
        out.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    }
    out
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
            app.plugins.on_event(&Event::Cooldown {
                key,
                kind: FailureKind::from(class),
            });
        }
        let cooling_secs = health
            .cooling_remaining(key, Instant::now())
            .map(|d| d.as_secs());
        tracing::warn!(
            stream,
            key,
            ?class,
            ?status,
            cooled,
            ?cooling_secs,
            "upstream key failed"
        );
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

async fn read_limited(mut response: UpstreamResponse) -> Bytes {
    let mut collected = Vec::new();
    while let Some(Ok(chunk)) = response.chunk().await {
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
    key: &str,
    response: UpstreamResponse,
    is_head: bool,
) -> Response {
    let mut status = response.status;
    let mut headers = HeaderMap::new();
    copy_response_headers(&response.headers, &mut headers);
    // Pass the length through when the upstream gave one, so non-chunked bodies stay non-chunked.
    if let Some(length) = response.headers.get(header::CONTENT_LENGTH) {
        headers.insert(header::CONTENT_LENGTH, length.clone());
    }
    let body = if app.plugins.is_empty() {
        Body::from_stream(response.into_stream())
    } else {
        let mut states = plugin_states;
        // A plugin that changes the body length must drop content-length here.
        app.plugins
            .on_response_head(&mut states, key, &mut status, &mut headers);
        // A plugin may change the body length, and a known length would also stop
        // the final `is_last` call from being made. These responses carry no body, so
        // their length is only metadata.
        let bodyless =
            is_head || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED;
        if !bodyless {
            headers.remove(header::CONTENT_LENGTH);
        }
        Body::from_stream(PluginBody {
            chain: Arc::clone(&app.plugins),
            states,
            inner: response.into_stream(),
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
    key: &str,
    status: StatusCode,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if chain.is_empty() {
        return buffered_response(status, headers, body);
    }
    let mut status = status;
    let mut headers = headers.clone();
    chain.on_response_head(states, key, &mut status, &mut headers);
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
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
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
                    return Poll::Ready(Some(Err(error)));
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
    fn prefixes_match_on_whole_segments() {
        assert!(has_prefix("/inference/openai/v1/x", "/inference"));
        assert!(has_prefix("/inference", "/inference/"));
        assert!(!has_prefix("/inference2/x", "/inference"));
        assert!(!has_prefix("/other", "/inference"));
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
    fn the_credential_leads_and_a_colliding_client_header_is_replaced() {
        let mut inbound = HeaderMap::new();
        inbound.insert("user-agent", HeaderValue::from_static("c/1"));
        inbound.insert("x-token", HeaderValue::from_static("client-value"));
        let out = upstream_headers(&inbound, "x-token", "upstream-value", false, false);
        let names: Vec<_> = out.keys().map(HeaderName::as_str).collect();
        assert_eq!(names.first(), Some(&"x-token"));
        assert_eq!(out["x-token"], "upstream-value");
        assert_eq!(out.get_all("x-token").iter().count(), 1);
    }

    #[test]
    fn keep_alive_is_sent_only_when_asked_for() {
        let mut inbound = HeaderMap::new();
        inbound.insert("connection", HeaderValue::from_static("close"));
        let without = upstream_headers(&inbound, "authorization", "Bearer k", false, false);
        assert!(without.get("connection").is_none());
        let with = upstream_headers(&inbound, "authorization", "Bearer k", false, true);
        assert_eq!(with["connection"], "keep-alive");
        let passthrough = passthrough_headers(&inbound, false, true);
        assert_eq!(passthrough["connection"], "keep-alive");
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
        inbound.insert(
            "accept-encoding",
            HeaderValue::from_static("gzip, deflate, br, zstd"),
        );
        let out = upstream_headers(
            &inbound,
            "authorization",
            "Bearer upstream-key",
            false,
            false,
        );
        assert_eq!(out["accept-encoding"], "identity");
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
    fn the_clients_accept_encoding_is_forwarded_only_when_asked() {
        let mut inbound = HeaderMap::new();
        inbound.insert(
            "accept-encoding",
            HeaderValue::from_static("gzip, deflate, br, zstd"),
        );
        let out = upstream_headers(&inbound, "authorization", "Bearer k", true, false);
        assert_eq!(out["accept-encoding"], "gzip, deflate, br, zstd");
        assert_eq!(out.get_all("accept-encoding").iter().count(), 1);

        // A client that sent none still gets plain responses, never a library default.
        let out = upstream_headers(&HeaderMap::new(), "authorization", "Bearer k", true, false);
        assert_eq!(out["accept-encoding"], "identity");

        let out = upstream_headers(&inbound, "authorization", "Bearer k", false, false);
        assert_eq!(out["accept-encoding"], "identity");
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
