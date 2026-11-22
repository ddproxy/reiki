use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use futures_core::Stream;

use crate::auth::{
    LoginThrottle, Peer, ThrottleSettings, constant_time_eq, extract_token, sha256_hex,
    strip_query_key,
};
use crate::chain::{Chain, KeyState, Policy, SavedHealth, Scope, scope_route};
use crate::classify::{Classification, ExtraPhrases, FailureClass, Outcome, classify_with};
use crate::config::{Config, DEFAULT_SCOPE_ESCALATE_AFTER, PeersConfig};
use crate::forward::Forwarder;
use crate::plugins::{
    PLUGIN_FAILURE_MESSAGE, PLUGIN_FAILURE_STATUS, PluginChain, PluginSession, RequestMeta,
};
use crate::pressure::PressureCounter;
use crate::rewrite::{request_model, rewrite_body};
use crate::shaper::bucket::{Bucket, BucketSnapshot, Permit, QuotaSnapshot};
use crate::shaper::headers::{Observed, observe};
use crate::shaper::{Acquire, KeyLimits, Shaper};
use crate::upstream::{Call, Faithful, Standard, Transport, UpstreamResponse};
use crate::usage::Usage;
use reiki_detect::body_is_encoded as is_encoded;
use reiki_plugin::{Event, FailureKind};

const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;
const FORWARDER_FAILED_BODY: &[u8] =
    b"{\"error\":{\"type\":\"bad_gateway\",\"message\":\"the upstream could not be reached\"}}";
const MAX_ERROR_BODY: usize = 64 * 1024;
const WITHHELD_NOTE: &str = "[body withheld: contains key material]";
const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
/// Stands in for the stream name when a request needs none.
pub(crate) const PASSTHROUGH_STREAM: &str = "passthrough";
/// Stands in for the key name on a request that used none of Reiki's keys.
const PASSTHROUGH_KEY: &str = "-";
const HEALTH_PATH: &str = "/__reiki/healthz";
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Source of the wall clock. Production reads the system clock; tests inject one they can pin.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

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

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug)]
pub enum LoadError {
    Read {
        key: String,
        source: std::io::Error,
    },
    Empty(String),
    Short {
        key: String,
        len: usize,
    },
    /// The key file can be read by its group or by others, and strict checks are on.
    Permissions {
        key: String,
        mode: u32,
    },
    /// The key file is longer than the scrubber can cover with bounded forms.
    Long {
        key: String,
        len: usize,
    },
}

impl LoadError {
    /// The configured key name. This is an identifier, never the contents of the key file.
    pub fn key(&self) -> &str {
        match self {
            Self::Read { key, .. }
            | Self::Empty(key)
            | Self::Short { key, .. }
            | Self::Permissions { key, .. }
            | Self::Long { key, .. } => key,
        }
    }

    /// A fixed description of the failure, safe to print on stderr: unlike `Display`, it carries
    /// no data read from the key file.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => "cannot read the key file",
            Self::Empty(_) => "the key file is empty",
            Self::Short { .. } => "the key is too short",
            Self::Permissions { .. } => "the key file is accessible to group or others",
            Self::Long { .. } => "the key is too long",
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { key, source } => write!(f, "cannot read key file for `{key}`: {source}"),
            Self::Empty(key) => write!(f, "key file for `{key}` is empty"),
            Self::Short { key, len } => write!(
                f,
                "key file for `{key}` holds {len} bytes; a key must be at least {} bytes so an echoed copy can be scrubbed",
                crate::scrub::MIN_SECRET
            ),
            Self::Permissions { key, mode } => write!(
                f,
                "key file for `{key}` has mode {mode:04o}; it is readable or writable by group or others (chmod 600, or set strict_key_permissions = false)"
            ),
            Self::Long { key, len } => write!(
                f,
                "key file for `{key}` holds {len} bytes; a key must be at most {} bytes so the scrubber's forms stay bounded",
                crate::scrub::MAX_SECRET
            ),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Empty(_) | Self::Short { .. } | Self::Permissions { .. } | Self::Long { .. } => {
                None
            }
        }
    }
}

/// Validates one key's contents and file mode without touching the filesystem. `load_keys` feeds
/// this the bytes and metadata it read from one handle; the fuzz target feeds it arbitrary text.
pub(crate) fn key_file_verdict(
    name: &str,
    raw: &str,
    mode: Option<u32>,
    strict: bool,
) -> Result<String, LoadError> {
    #[cfg(unix)]
    if let Some(mode) = mode
        && let Some(crate::keys::CheckIssue::GroupOrWorldReadable(_)) =
            crate::keys::permission_verdict(mode)
    {
        if strict {
            return Err(LoadError::Permissions {
                key: name.to_owned(),
                mode,
            });
        }
        tracing::warn!(
            key = %name,
            "key file has group or world read/write bits (mode {mode:04o}); set strict_key_permissions = true to refuse it"
        );
    }
    #[cfg(not(unix))]
    let _ = (mode, strict);

    let value = raw.trim();
    if value.is_empty() {
        return Err(LoadError::Empty(name.to_owned()));
    }
    if value.len() < crate::scrub::MIN_SECRET {
        return Err(LoadError::Short {
            key: name.to_owned(),
            len: value.len(),
        });
    }
    if value.len() > crate::scrub::MAX_SECRET {
        return Err(LoadError::Long {
            key: name.to_owned(),
            len: value.len(),
        });
    }
    Ok(value.to_owned())
}

pub fn load_keys(config: &Config) -> Result<HashMap<String, Secret>, LoadError> {
    let mut keys = HashMap::new();
    for (name, key) in &config.keys {
        // Metadata and contents come from one handle, so a swap between the mode check and
        // the read cannot substitute a private file for a loose one.
        let mut handle = std::fs::File::open(&key.file).map_err(|source| LoadError::Read {
            key: name.clone(),
            source,
        })?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;

            Some(
                handle
                    .metadata()
                    .map_err(|source| LoadError::Read {
                        key: name.clone(),
                        source,
                    })?
                    .permissions()
                    .mode()
                    & 0o777,
            )
        };
        #[cfg(not(unix))]
        let mode = None;
        let mut raw = String::new();
        std::io::Read::read_to_string(&mut handle, &mut raw).map_err(|source| LoadError::Read {
            key: name.clone(),
            source,
        })?;
        let value = key_file_verdict(name, &raw, mode, config.strict_key_permissions)?;
        keys.insert(name.clone(), Secret::new(value));
    }
    Ok(keys)
}

/// Every key whose upstream has a `limits` table, with the window it shapes.
/// Config validation has already rejected unusable tables; a stray error here
/// leaves that key unshaped rather than taking the proxy down.
fn shaped_keys(config: &Config) -> BTreeMap<String, KeyLimits> {
    config
        .keys
        .iter()
        .filter_map(|(name, key)| {
            let limits = config.upstream.get(&key.upstream)?.limits.as_ref()?;
            match limits.to_window() {
                Ok((window, max_wait)) => Some((
                    name.clone(),
                    KeyLimits {
                        window,
                        max_wait,
                    },
                )),
                Err(error) => {
                    tracing::error!(key = %name, upstream = %key.upstream, %error, "ignoring unusable limits");
                    None
                }
            }
        })
        .collect()
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
        let mut limiter = self
            .limiters
            .get(stream)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        limiter.check(now).err()
    }

    pub(crate) fn authenticate(&self, token: &str) -> Option<&str> {
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
        scope: Option<&Scope>,
    ) -> Option<String> {
        let health = health.lock().unwrap_or_else(PoisonError::into_inner);
        let mut chain = self
            .chains
            .get(stream)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let keys = &self.config.keys;
        chain
            .pick_where_scoped(&health, Instant::now(), scope, |k| {
                !skip.iter().any(|s| s == k) && keys.get(k).is_some_and(|c| c.upstream == upstream)
            })
            .map(str::to_owned)
    }

    fn soonest_recovery(
        &self,
        health: &Mutex<KeyState>,
        stream: &str,
        upstream: &str,
        scope: Option<&Scope>,
    ) -> Option<Duration> {
        let health = health.lock().unwrap_or_else(PoisonError::into_inner);
        let chain = self
            .chains
            .get(stream)?
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let keys = &self.config.keys;
        health.soonest_recovery_for(chain.keys(), scope, Instant::now(), |key| {
            keys.get(key).is_some_and(|key| key.upstream == upstream)
        })
    }
}

/// Pause before a quota 429 is sent again on the same key when `confirm_quota` is set.
const QUOTA_CONFIRM_DELAY: Duration = Duration::from_secs(3);

pub struct App {
    snapshot: RwLock<Arc<Snapshot>>,
    health: Mutex<KeyState>,
    /// Per-upstream attempts and tokens for the last hour, in memory only. Shared with the
    /// response bodies, which record their tokens when they end.
    pressure: Arc<PressureCounter>,
    /// The latest key failures, across streams, for `/v1/failures` and `reiki top`.
    failures: crate::failures::FailureRing,
    /// Per-key outbound buckets, kept across reloads for an unchanged window.
    shaper: Shaper,
    /// The shaper snapshot last written and when, so natural expiry of a pause or
    /// closed window is not mistaken for a change.
    shaper_saved: Mutex<(BTreeMap<String, BucketSnapshot>, Option<SystemTime>)>,
    policy: Policy,
    standard: Standard,
    faithful: Faithful,
    forwarder: Forwarder,
    header_timeout: Duration,
    state_path: OnceLock<PathBuf>,
    /// Held across snapshot and write, so a stale snapshot cannot land after a newer one.
    persist_lock: Mutex<()>,
    /// Wakes the state writer thread; absent until [`App::start_state_writer`] runs.
    persist_tx: OnceLock<std::sync::mpsc::SyncSender<()>>,
    /// Where wall-clock reads come from; production uses [`SystemClock`].
    clock: Arc<dyn Clock>,
    /// When the interception CA expires, as Unix seconds; unset without interception.
    ca_not_after: RwLock<Option<i64>>,
    logins: LoginThrottle,
    /// Separate counts, so failed admin logins cannot lock out client requests from the same peer.
    admin_logins: LoginThrottle,
    /// Replaced by a reload; a request holds the chain it started with through its [`PluginSession`].
    plugins: RwLock<Arc<PluginChain>>,
    /// The tunnel, once started, so a reload can change its policy.
    tunnel: OnceLock<Arc<crate::tunnel::Tunnel>>,
    /// Where the config was read from, for a reload that is not tied to a file argument.
    config_path: OnceLock<PathBuf>,
    /// The service log, so a reload can apply the `[log]` level.
    logging: OnceLock<Arc<crate::logging::Logging>>,
    reload_state: Mutex<crate::reload::ReloadState>,
    /// Held for the whole of a reload, so two reloads never interleave their steps.
    reload_lock: Mutex<()>,
    /// Write-held while a reload swaps the snapshot, plugin chain and tunnel policy; a request
    /// takes its snapshot and plugin session under a read hold so it sees one config.
    swap: std::sync::RwLock<()>,
    /// Set only when the peer listener is started.
    #[cfg(feature = "peers")]
    peer_publisher: OnceLock<Arc<crate::peers::Publisher>>,
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
        Self::with_header_timeout_and_clock(
            config,
            keys,
            policy,
            header_timeout,
            Arc::new(SystemClock),
        )
    }

    /// Builds an app whose clock the caller controls; useful for deterministic tests.
    pub fn with_header_timeout_and_clock(
        config: Config,
        keys: HashMap<String, Secret>,
        policy: Policy,
        header_timeout: Duration,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        Self::with_plugins_and_clock(
            config,
            keys,
            policy,
            header_timeout,
            PluginChain::default(),
            clock,
        )
    }

    pub fn with_plugins(
        config: Config,
        keys: HashMap<String, Secret>,
        policy: Policy,
        header_timeout: Duration,
        plugins: PluginChain,
    ) -> Arc<Self> {
        Self::with_plugins_and_clock(
            config,
            keys,
            policy,
            header_timeout,
            plugins,
            Arc::new(SystemClock),
        )
    }

    pub fn with_plugins_and_clock(
        config: Config,
        keys: HashMap<String, Secret>,
        policy: Policy,
        header_timeout: Duration,
        plugins: PluginChain,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        let throttle = ThrottleSettings::from(&config.auth);
        let shaper = Shaper::new();
        shaper.sync(&shaped_keys(&config));
        Arc::new(Self {
            snapshot: RwLock::new(Arc::new(Snapshot::new(config, keys))),
            health: Mutex::new(KeyState::default()),
            pressure: Arc::new(PressureCounter::new()),
            failures: crate::failures::FailureRing::new(),
            shaper,
            shaper_saved: Mutex::new((BTreeMap::new(), None)),
            policy,
            standard: Standard::try_new().expect("cannot build the upstream HTTP client"),
            faithful: Faithful::new(),
            forwarder: Forwarder::default(),
            header_timeout,
            state_path: OnceLock::new(),
            persist_lock: Mutex::new(()),
            persist_tx: OnceLock::new(),
            clock,
            ca_not_after: RwLock::new(None),
            logins: LoginThrottle::new(throttle),
            admin_logins: LoginThrottle::new(throttle),
            plugins: RwLock::new(Arc::new(plugins)),
            tunnel: OnceLock::new(),
            config_path: OnceLock::new(),
            logging: OnceLock::new(),
            reload_state: Mutex::new(crate::reload::ReloadState::new()),
            reload_lock: Mutex::new(()),
            swap: std::sync::RwLock::new(()),
            #[cfg(feature = "peers")]
            peer_publisher: OnceLock::new(),
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

    /// The chain new requests start on.
    pub(crate) fn plugins(&self) -> Arc<PluginChain> {
        let guard = self.plugins.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }

    /// Starts a request's plugin session on the current chain.
    /// The snapshot and plugin session for one request, taken together so a reload cannot land
    /// between them.
    pub(crate) fn view(&self) -> (Arc<Snapshot>, PluginSession) {
        let _held = self.swap.read().unwrap_or_else(PoisonError::into_inner);
        (self.snapshot(), self.session())
    }

    pub(crate) fn session(&self) -> PluginSession {
        self.plugins().session()
    }

    pub(crate) fn health(&self) -> &Mutex<KeyState> {
        &self.health
    }

    pub(crate) fn pressure(&self) -> &PressureCounter {
        &self.pressure
    }

    /// The in-memory ring of recent key failures.
    pub(crate) fn failures(&self) -> &crate::failures::FailureRing {
        &self.failures
    }

    /// Failed client and intercepted-CONNECT token checks share one count.
    pub(crate) fn logins(&self) -> &LoginThrottle {
        &self.logins
    }

    /// Failed admin token checks, kept apart from the client listener's counts.
    pub(crate) fn admin_logins(&self) -> &LoginThrottle {
        &self.admin_logins
    }

    /// The peer publisher, created on first use by the peer listener.
    #[cfg(feature = "peers")]
    pub(crate) fn peer_publisher(&self) -> &Arc<crate::peers::Publisher> {
        self.peer_publisher.get_or_init(|| {
            let throttle = ThrottleSettings::from(&self.snapshot().config.auth);
            crate::peers::Publisher::new(throttle)
        })
    }

    #[cfg(feature = "peers")]
    pub(crate) fn config_path(&self) -> Option<&PathBuf> {
        self.config_path.get()
    }

    #[cfg(feature = "peers")]
    pub(crate) fn state_path(&self) -> Option<&PathBuf> {
        self.state_path.get()
    }

    /// Point the app at the file it persists key health in. Set once at startup.
    pub fn set_state_path(&self, path: PathBuf) {
        let _ = self.state_path.set(path);
    }

    /// Records the interception CA expiry, which a CA swap on reload updates.
    pub fn set_ca_not_after(&self, unix_seconds: i64) {
        *self
            .ca_not_after
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(unix_seconds);
    }

    pub(crate) fn ca_not_after(&self) -> Option<i64> {
        *self
            .ca_not_after
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn wall_now(&self) -> SystemTime {
        self.clock.now()
    }

    /// Apply persisted key health and shaper state. Missing or corrupt state is ignored.
    pub fn load_persisted_state(&self) {
        let Some(path) = self.state_path.get() else {
            return;
        };
        let config = self.snapshot();
        let loaded = crate::state::load_state(path, &config.config);
        let now = self.wall_now();
        self.shaper.restore(&loaded.shaper, now);
        self.restore_state(loaded.health);
        *self
            .shaper_saved
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = (self.shaper.snapshot(now), Some(now));
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

    /// Starts the thread that writes the state file. From then on [`App::persist_state`] only
    /// wakes it, so a request that records a failure never waits on a disk. A wake that arrives
    /// while a write runs is kept and becomes one more write that reads the latest state, so
    /// a burst of failures costs a few writes, not one each.
    pub fn start_state_writer(self: &Arc<Self>) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        if self.persist_tx.set(tx).is_err() {
            return;
        }
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("reiki-state".to_owned())
            .spawn(move || {
                while rx.recv().is_ok() {
                    let Some(app) = weak.upgrade() else {
                        break;
                    };
                    app.persist_state_now();
                }
            });
        if let Err(error) = spawned {
            // The receiver went with the closure, so `persist_state` falls back to writing inline.
            tracing::warn!(%error, "cannot start the state writer; writing inline");
        }
    }

    /// Saves the state, by waking the writer thread when there is one.
    fn persist_state(&self) {
        if let Some(tx) = self.persist_tx.get() {
            match tx.try_send(()) {
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => return,
                Err(std::sync::mpsc::TrySendError::Disconnected(())) => {}
            }
        }
        self.persist_state_now();
    }

    /// Tells the plugins the process is stopping. Blocks while a plugin writes out what it holds.
    pub fn shutdown_plugins(&self) {
        let chain = Arc::clone(&self.plugins.read().unwrap_or_else(PoisonError::into_inner));
        chain.shutdown();
    }

    /// Writes the state now and waits for it. Used at shutdown, so a queued wake is not lost.
    pub fn flush_state(&self) {
        self.persist_state_now();
    }

    fn persist_state_now(&self) {
        let Some(path) = self.state_path.get() else {
            return;
        };
        let _guard = self
            .persist_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let health = self
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot(Instant::now());
        let now = self.wall_now();
        let shaper = self.shaper.snapshot(now);
        let config = self.snapshot();
        crate::state::save_with_shaper(path, &health, &shaper, &config.keys, &config.config);
        *self
            .shaper_saved
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = (shaper, Some(now));
    }

    /// Saves after the shaper learns something, skipping a write when its durable
    /// facts are unchanged.
    fn persist_shaper(&self) {
        if self.state_path.get().is_none() {
            return;
        }
        let now = self.wall_now();
        let shaper = self.shaper.snapshot(now);
        {
            let saved = self
                .shaper_saved
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(saved_at) = saved.1
                && shaper_unchanged(&saved.0, saved_at, now, &shaper)
            {
                return;
            }
        }
        self.persist_state();
    }

    /// Re-enables a key that was taken out of rotation and persists the change. Returns false
    /// when no such key is configured.
    pub fn enable_key(&self, name: &str) -> bool {
        {
            // A reload prunes health under this lock, so a key removed meanwhile is not revived.
            let mut health = self.health.lock().unwrap_or_else(PoisonError::into_inner);
            if !self.snapshot().config.keys.contains_key(name) {
                return false;
            }
            health.enable(name);
        }
        self.persist_state();
        true
    }

    /// Swap in a new config and key set. A request keeps the snapshot it started
    /// with. Plugins are built at startup and are not rebuilt here.
    pub fn reload(&self, config: Config, keys: HashMap<String, Secret>) {
        let throttle = ThrottleSettings::from(&config.auth);
        let wanted_shapes = shaped_keys(&config);
        let next = Arc::new(Snapshot::new(config, keys));
        // Health first, then the snapshot: `record` takes them in the same order, so a
        // failure from a request on the old snapshot cannot land after the re-enable.
        let mut health = self.health.lock().unwrap_or_else(PoisonError::into_inner);
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        health.retain_keys(|name| next.keys.contains_key(name));
        for (name, secret) in &next.keys {
            let unchanged = current.keys.get(name).is_some_and(|old| old.0 == secret.0);
            if !unchanged {
                health.enable(name);
            }
        }
        self.shaper.sync(&wanted_shapes);
        *current = next;
        self.logins.configure(throttle);
        self.admin_logins.configure(throttle);
        #[cfg(feature = "peers")]
        if let Some(publisher) = self.peer_publisher.get() {
            publisher.logins().configure(throttle);
        }
    }

    /// Hands the app the service log so a reload applies the `[log]` level.
    pub fn set_logging(&self, logging: Arc<crate::logging::Logging>) {
        let _ = self.logging.set(logging);
    }

    /// Hands the app the running tunnel so a reload can change its policy.
    pub fn set_tunnel(&self, tunnel: Arc<crate::tunnel::Tunnel>) {
        let _ = self.tunnel.set(tunnel);
    }

    /// Every file whose change should trigger a reload: the config, the key files, the interception CA files and the files plugins read.
    pub fn reload_inputs(&self) -> Vec<PathBuf> {
        let snapshot = self.snapshot();
        let mut paths: Vec<PathBuf> = self.config_path.get().cloned().into_iter().collect();
        paths.extend(
            snapshot
                .config
                .keys
                .values()
                .map(|key| PathBuf::from(&key.file)),
        );
        if let Some(intercept) = snapshot
            .config
            .tunnel
            .as_ref()
            .and_then(|tunnel| tunnel.intercept.as_ref())
        {
            paths.push(PathBuf::from(&intercept.ca_cert));
            paths.push(PathBuf::from(&intercept.ca_key));
        }
        if let Some((cert, key)) = snapshot
            .config
            .peers
            .as_ref()
            .filter(|peers| peers.publishing())
            .and_then(PeersConfig::tls_files)
        {
            paths.push(PathBuf::from(cert));
            paths.push(PathBuf::from(key));
        }
        paths.extend(self.plugins().watched_paths());
        paths.sort();
        paths.dedup();
        paths
    }

    /// Records the config path that `POST /v1/reload` re-reads.
    pub fn set_config_path(&self, path: PathBuf) {
        let _ = self.config_path.set(path);
    }

    /// The `config` object of `/v1/status`: the reload generation, when it loaded and how the last reload went.
    pub fn reload_status_json(&self) -> String {
        self.reload_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status_json()
    }

    /// Failed service-log file writes dropped since start, for `/v1/status`.
    pub(crate) fn file_write_errors(&self) -> u64 {
        self.logging
            .get()
            .and_then(|logging| logging.file_write_errors())
            .unwrap_or(0)
    }

    /// The SIGHUP action. Live health is authoritative once running, so the state
    /// file is deliberately not replayed here: it would revive the disabled flag
    /// of a key whose secret was just rotated.
    pub fn reload_on_hangup(&self, path: &Path) {
        match self.reload_from_file(path) {
            Ok(report) => tracing::info!(
                generation = report.generation,
                "reloaded: {}",
                report.summary()
            ),
            Err(error) => {
                tracing::warn!(%error, "reload failed; keeping the current config");
                // A rotation tool signals the process; the file must still move.
                if let Some(logging) = self.logging.get() {
                    logging.reopen();
                }
            }
        }
    }

    /// Reloads from the path given to [`App::set_config_path`].
    pub fn reload_now(&self) -> Result<crate::reload::ReloadReport, String> {
        let Some(path) = self.config_path.get() else {
            return Err("the config path is not known to this instance".to_owned());
        };
        let result = self.reload_from_file(path);
        match &result {
            Ok(report) => tracing::info!(
                generation = report.generation,
                "reloaded: {}",
                report.summary()
            ),
            Err(error) => tracing::warn!(%error, "reload failed; keeping the current config"),
        }
        result
    }

    /// Re-reads the config and key files and applies what can change live: keys, streams,
    /// upstreams, plugin settings, the tunnel policy and the intercept host table. Everything is
    /// built and checked first, so a failure at any step leaves the running state untouched.
    pub fn reload_from_file(&self, path: &Path) -> Result<crate::reload::ReloadReport, String> {
        let _serial = self
            .reload_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let result = self.try_reload(path, true);
        let mut state = self
            .reload_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &result {
            Ok(report) => state.record_ok(report),
            Err(error) => state.record_error(error),
        }
        result
    }

    /// What a reload from the configured path would change, without applying or recording anything.
    pub fn preview_reload(&self) -> Result<crate::reload::ReloadReport, String> {
        let Some(path) = self.config_path.get() else {
            return Err("the config path is not known to this instance".to_owned());
        };
        self.try_reload(path, false)
    }

    fn try_reload(&self, path: &Path, apply: bool) -> Result<crate::reload::ReloadReport, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let config = Config::parse(&text).map_err(|e| format!("config: {e}"))?;
        let keys = load_keys(&config).map_err(|e| e.to_string())?;
        // Dry-run the plugin settings first: the real build below opens files and sockets.
        let dry = crate::plugins::with_dry_run(&config.plugins);
        crate::plugins::PluginChain::build(&dry, &crate::plugins::builtin())
            .map_err(|e| format!("config: {e}"))?;
        let registry = crate::plugins::builtin();
        let rebuilt = if apply {
            self.plugins().rebuild(&config.plugins, &registry)
        } else {
            self.plugins().plan(&config.plugins, &registry)
        }
        .map_err(|e| format!("config: {e}"))?;
        let tunnel_plan = match (self.tunnel.get(), &config.tunnel) {
            (Some(tunnel), Some(wanted)) => Some(
                tunnel
                    .plan_reload(wanted)
                    .map_err(|e| format!("config: {e}"))?,
            ),
            _ => None,
        };

        let mut changed = Vec::new();
        let mut restart_needed: Vec<String> = rebuilt
            .restart_needed
            .iter()
            .map(|name| format!("plugins.{name}"))
            .collect();
        if !rebuilt.changed.is_empty() {
            changed.push(format!("plugins {}", rebuilt.changed.join(",")));
        }
        let current = self.snapshot();
        if current.config.listen != config.listen {
            restart_needed.push("listen".to_owned());
        }
        let admin_listen = |c: &Config| c.admin.as_ref().map(|a| a.listen.clone());
        if admin_listen(&current.config) != admin_listen(&config) {
            restart_needed.push("admin.listen".to_owned());
        }
        restart_needed.extend(current.config.server.restart_needed(&config.server));
        if current.config.service != config.service {
            restart_needed.push("service.shutdown_timeout_secs".to_owned());
        }
        let peer_listen = |c: &Config| {
            c.peers
                .as_ref()
                .filter(|p| p.publishing())
                .and_then(|p| p.listen.clone())
        };
        if peer_listen(&current.config) != peer_listen(&config) {
            restart_needed.push("peers.listen".to_owned());
        }
        #[cfg(feature = "peers")]
        let peer_tls = match self.peer_publisher.get() {
            Some(publisher) => publisher
                .plan_tls(config.peers.as_ref())
                .map_err(|e| format!("peers: {e}"))?,
            None => crate::peers::TlsReload::default(),
        };
        #[cfg(feature = "peers")]
        {
            if peer_tls.restart {
                restart_needed.push("peers.tls".to_owned());
            }
            if peer_tls.swaps() {
                changed.push("peers.tls".to_owned());
            }
        }
        let tunnel_listen = |c: &Config| c.tunnel.as_ref().map(|t| t.listen.clone());
        if tunnel_listen(&current.config) != tunnel_listen(&config) {
            restart_needed.push("tunnel.listen".to_owned());
        }
        if let Some(logging) = self.logging.get() {
            restart_needed.extend(logging.restart_needed(&config.log));
        }
        match (self.tunnel.get(), &config.tunnel) {
            (Some(_), None) => restart_needed.push("tunnel removed".to_owned()),
            (None, Some(_)) => restart_needed.push("tunnel added".to_owned()),
            _ => {}
        }
        if let Some(plan) = &tunnel_plan {
            changed.extend(plan.changed.iter().cloned());
            restart_needed.extend(plan.restart_needed.iter().cloned());
        }
        drop(current);

        let (key_count, stream_count) = (keys.len(), config.streams.len());
        let next_generation = self
            .reload_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_generation();
        if !apply {
            return Ok(crate::reload::ReloadReport {
                generation: next_generation - 1,
                changed,
                restart_needed,
                keys: key_count,
                streams: stream_count,
                dry_run: true,
            });
        }
        let generation = next_generation;
        // Nothing below can fail, so the new state lands whole or not at all.
        let log = config.log.clone();
        let swap_guard = self.swap.write().unwrap_or_else(PoisonError::into_inner);
        self.reload(config, keys);
        *self.plugins.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(rebuilt.chain);
        if let (Some(tunnel), Some(plan)) = (self.tunnel.get(), tunnel_plan) {
            tunnel.apply_reload(plan);
        }
        #[cfg(feature = "peers")]
        if let Some(publisher) = self.peer_publisher.get() {
            publisher.apply_tls(peer_tls);
        }
        drop(swap_guard);
        if let Some(logging) = self.logging.get() {
            logging.reload(&log);
        }
        Ok(crate::reload::ReloadReport {
            generation,
            changed,
            restart_needed,
            keys: key_count,
            streams: stream_count,
            dry_run: false,
        })
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

/// How many percent-decoding layers are inspected. Each valid decode shortens the path, so a
/// path still changing after this many layers cannot be checked and is refused.
const MAX_PATH_DECODE_LAYERS: usize = 8;

/// True for a `.` or `..` segment, plain or percent-encoded, an encoded `/` or `\`, or a
/// raw backslash: forms an upstream may normalise into a path outside the one Reiki checked.
/// Every encoding layer is checked, so `%252e%252e` is caught once it decodes to `%2e%2e`.
pub(crate) fn has_path_trick(path: &str) -> bool {
    let mut layer = path.to_owned();
    for _ in 0..MAX_PATH_DECODE_LAYERS {
        let lower = layer.to_ascii_lowercase();
        if lower.contains("%2f")
            || lower.contains("%5c")
            || lower.contains('\\')
            || lower
                .split('/')
                .any(|segment| matches!(segment.replace("%2e", ".").as_str(), "." | ".."))
        {
            return true;
        }
        // A malformed escape, or bytes that are not UTF-8 after decoding, cannot be validated:
        // refuse the path instead of forwarding something an upstream might normalise differently.
        let Some(decoded) = decode_path_layer(&layer) else {
            return true;
        };
        if decoded == layer {
            return false;
        }
        layer = decoded;
    }
    // Still changing after the last layer: the check could not be completed.
    true
}

/// One pass of percent-decoding, or `None` for a malformed escape or non-UTF-8 result.
pub(crate) fn decode_path_layer(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = |byte: u8| -> Option<u8> {
                match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                }
            };
            let high = hex(*bytes.get(at + 1)?)?;
            let low = hex(*bytes.get(at + 2)?)?;
            out.push((high << 4) | low);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
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
pub(crate) fn json_escape(text: &str) -> String {
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

pub(crate) fn split_upstream(path: &str) -> Option<(&str, &str)> {
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
    let (snapshot, plugin_session) = app.view();

    // The client's own credentials go upstream, so there is no Reiki token to check.
    let passthrough = split_upstream(parts.uri.path())
        .and_then(|(name, _)| snapshot.config.upstream.get(name))
        .is_some_and(|u| u.passthrough);
    let stream = if passthrough {
        PASSTHROUGH_STREAM.to_owned()
    } else {
        let peer = parts.extensions.get::<Peer>().map(|p| p.0);
        // Check and reserve in one lock, so a burst cannot all pass before any attempt is counted.
        let attempt = peer.map(|ip| app.logins.reserve(ip, Instant::now()));
        if let Some(Err(wait)) = attempt {
            tracing::warn!(
                ?peer,
                path = %parts.uri.path(),
                "too many failed logins, refusing"
            );
            return rate_limited_response(wait);
        }
        let fail = |reason: &str| {
            let failures = attempt.and_then(Result::ok).map(|a| a.failures());
            tracing::warn!(?peer, reason, ?failures, path = %parts.uri.path(), "failed login");
            json_error(StatusCode::UNAUTHORIZED, "unauthorized", reason)
        };
        let Some(token) = extract_token(&parts.headers, parts.uri.query()) else {
            return fail("missing token");
        };
        let Some(stream) = snapshot.authenticate(&token) else {
            return fail("unknown token");
        };
        if let Some(ip) = peer {
            app.logins.succeed(ip);
        }
        stream.to_owned()
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
        plugin_session,
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

    let (snapshot, plugin_session) = app.view();
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
        plugin_session,
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
    plugin_session: PluginSession,
) -> Response {
    use tracing::Instrument;
    #[cfg(feature = "peers")]
    if let (Some(publisher), Some(peers)) = (app.peer_publisher.get(), &snapshot.config.peers)
        && stream != PASSTHROUGH_STREAM
    {
        publisher.observe_session(peers, stream, &parts.headers);
    }
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
        plugin_session,
    )
    .instrument(span)
    .await
}

/// Emits `RequestDone` exactly once. If the request task is dropped before it can finish
/// (a client disconnect or an early return), it reports 499, the conventional status for a
/// client closing the request, so plugins can release per-request accounting.
struct RequestDone<'a> {
    plugins: Arc<PluginChain>,
    stream: &'a str,
    upstream: String,
    status: Option<u16>,
}

impl<'a> RequestDone<'a> {
    fn new(plugins: Arc<PluginChain>, stream: &'a str, upstream: &str) -> Self {
        Self {
            plugins,
            stream,
            upstream: upstream.to_owned(),
            status: None,
        }
    }

    fn set_upstream(&mut self, upstream: &str) {
        self.upstream = upstream.to_owned();
    }

    fn finish(mut self, status: u16) {
        self.status = Some(status);
        self.emit(status);
    }

    fn emit(&self, status: u16) {
        self.plugins.on_event(&Event::RequestDone {
            stream: self.stream,
            upstream: &self.upstream,
            status,
        });
    }
}

impl Drop for RequestDone<'_> {
    fn drop(&mut self) {
        if self.status.is_none() {
            self.emit(499);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_inner(
    app: &App,
    snapshot: &Snapshot,
    stream: &str,
    upstream_name: &str,
    upstream: &crate::config::Upstream,
    rest: String,
    parts: Parts,
    body: Body,
    id: &str,
    faithful: bool,
    plugin_session: PluginSession,
) -> Response {
    // Dropped before a response: reports 499.
    let mut request_done = RequestDone::new(plugin_session.chain().clone(), stream, upstream_name);
    let response = dispatch_body(
        app,
        snapshot,
        stream,
        upstream_name,
        upstream,
        rest,
        parts,
        body,
        id,
        faithful,
        plugin_session,
        &mut request_done,
    )
    .await;
    request_done.finish(response.status().as_u16());
    response
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_body(
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
    plugin_session: PluginSession,
    request_done: &mut RequestDone<'_>,
) -> Response {
    let rest = rest.as_str();
    if has_path_trick(parts.uri.path()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "bad_path",
            "dot segments and encoded separators are not allowed in the path",
        );
    }
    let body =
        match tokio::time::timeout(app.header_timeout, to_bytes(body, MAX_REQUEST_BODY)).await {
            Ok(Ok(body)) => body,
            Ok(Err(_)) => {
                return json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    "request body too large",
                );
            }
            Err(_) => {
                return json_error(
                    StatusCode::REQUEST_TIMEOUT,
                    "body_timeout",
                    "the request body did not arrive in time",
                );
            }
        };
    let mut body = body;

    // Run request hooks once, before the failover loop, so every attempt sees
    // the same transformed request.
    let mut plugin_states = plugin_session;
    let meta = RequestMeta {
        id,
        stream,
        upstream: upstream_name,
        method: &parts.method,
        path: parts.uri.path(),
    };
    if let Err(blocked) = plugin_states
        .on_request_blocking(&meta, &mut parts.headers, &mut body)
        .await
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

    // Both the rewriter and model-scoped key selection read top-level members. Past the scan cap
    // they would give up; forwarding the original body would silently skip both, so refuse it.
    let rewrites = !upstream.model_aliases.is_empty() || !upstream.drop_params.is_empty();
    if (rewrites || !upstream.passthrough) && crate::rewrite::over_member_cap(&body) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "too_many_members",
            "the request body has too many top-level members",
        );
    }

    // Rewrite once, before the failover loop, so every key replay sends the same
    // bytes. reqwest sets content-length from the rewritten body on each send.
    let rewritten = rewrite_body(body.clone(), &upstream.model_aliases, &upstream.drop_params);

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
    let mut serving_upstream = upstream_name;
    let mut result = if upstream.passthrough {
        forward_passthrough(app, first, rewritten).await
    } else {
        forward(app, first, rewritten).await
    };

    // One hop only: when nothing is left to try here, hand the request to the
    // fallback upstream, re-mapped for it, instead of failing the client.
    if let (Err(spent), Some(fallback)) = (&mut result, &upstream.fallback)
        && let Some(target) = snapshot
            .config
            .upstream
            .get(&fallback.upstream)
            .filter(|_| !spent.link_failed)
    {
        tracing::warn!(stream = %stream, from = upstream_name, to = %fallback.upstream, "falling back");
        let body = rewrite_body(body, &fallback.model_aliases, &[]);
        let body = rewrite_body(body, &target.model_aliases, &target.drop_params);
        let used = fallback.upstream.as_str();
        serving_upstream = used;
        request_done.set_upstream(used);
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
    match result {
        Ok(response) => response,
        Err(spent) => match spent.refusal {
            Some((key, status, headers, body)) => {
                let mut session = spent.plugin_states;
                finish_buffered(&mut session, &key, serving_upstream, status, &headers, body)
            }
            None => exhausted(
                snapshot,
                &app.health,
                stream,
                serving_upstream,
                spent.scope.as_ref(),
            ),
        },
    }
}

/// A request whose chain ran dry; the plugin state lets a fallback continue it.
struct Spent {
    refusal: Option<(String, StatusCode, HeaderMap, Bytes)>,
    plugin_states: PluginSession,
    /// The route-and-model combination of the request that ran out of keys.
    scope: Option<Scope>,
    /// The forwarder link failed, which is final: no fallback upstream is tried.
    link_failed: bool,
}

fn forwarder_failed(key: String, plugin_states: PluginSession, scope: Option<Scope>) -> Box<Spent> {
    Box::new(Spent {
        refusal: Some((
            key,
            StatusCode::BAD_GATEWAY,
            HeaderMap::new(),
            Bytes::from_static(FORWARDER_FAILED_BODY),
        )),
        plugin_states,
        scope,
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
    plugin_states: PluginSession,
    faithful: bool,
}

fn remember_wait(smallest: &mut Option<Duration>, wait: Duration) {
    *smallest = Some(smallest.map_or(wait, |current| current.min(wait)));
}

/// True when `now`'s snapshot holds the same facts as the one saved at `saved_at`;
/// a pause or closed window simply running down is not a change.
/// How long quota counters may drift from the saved copy before they are saved again.
const COUNTER_SAVE_INTERVAL: Duration = Duration::from_secs(60);

fn shaper_unchanged(
    saved: &BTreeMap<String, BucketSnapshot>,
    saved_at: SystemTime,
    now: SystemTime,
    current: &BTreeMap<String, BucketSnapshot>,
) -> bool {
    if saved.len() != current.len() {
        return false;
    }
    let elapsed = now.duration_since(saved_at).unwrap_or_default();
    saved.iter().all(|(key, old)| {
        current.get(key).is_some_and(|new| {
            old.learned_rpm == new.learned_rpm
                && same_counters(old.daily, new.daily, elapsed)
                && same_counters(old.monthly, new.monthly, elapsed)
                && same_remaining(old.pause, new.pause, elapsed)
                && same_remaining(old.closed, new.closed, elapsed)
        })
    })
}

/// Quota counters move with every request, so only a new window or a long enough gap counts
/// as a change; otherwise every success on a daily-limited key would rewrite the state file.
fn same_counters(
    saved: Option<QuotaSnapshot>,
    current: Option<QuotaSnapshot>,
    elapsed: Duration,
) -> bool {
    match (saved, current) {
        (None, None) => true,
        (Some(old), Some(new)) => {
            old.start == new.start && (old == new || elapsed < COUNTER_SAVE_INTERVAL)
        }
        _ => false,
    }
}

fn same_remaining(saved: Option<Duration>, current: Option<Duration>, elapsed: Duration) -> bool {
    match saved {
        None => current.is_none(),
        Some(saved) => {
            let expected = saved.saturating_sub(elapsed);
            // A window that has run out is a change, so the stale entry is cleared.
            !expected.is_zero() && current == Some(expected)
        }
    }
}

/// A bucket has no pause-only call, so a response that reports zero remaining
/// closes the bucket until its reset.
fn note_reported_remaining(bucket: &Bucket, observed: Observed, now: SystemTime) {
    for (remaining, reset) in [
        (observed.requests_remaining, observed.requests_reset),
        (observed.tokens_remaining, observed.tokens_reset),
    ] {
        if remaining == Some(0)
            && let Some(until) = reset.and_then(|reset| now.checked_add(reset))
        {
            bucket.note_window_closed(now, until);
        }
    }
}

fn shaped_response(wait: Duration) -> Response {
    let mut response = json_error(
        StatusCode::TOO_MANY_REQUESTS,
        "reiki_shaped",
        "no key may be used within the configured wait",
    );
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    if let Ok(value) = HeaderValue::from_str(&secs.clamp(1, 3600).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response.headers_mut().insert(
        HeaderName::from_static("x-reiki-origin"),
        HeaderValue::from_static("local"),
    );
    response
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

    let url = target_url(upstream, rest, parts.uri.query().and_then(strip_query_key));
    // The combination failures are confined to. It is derived from the rewritten body every
    // key sends, so a fallback that remaps the model gets its own scope.
    let scope = request_model(&body).map(|model| Scope {
        route: scope_route(rest),
        model,
    });

    // Enough attempts to walk the chain once, plus one retry of a transient failure.
    let max_attempts = snapshot
        .chains
        .get(stream)
        .map_or(1, |_| snapshot.config.streams[stream].chain.len() + 1);

    let mut skip: Vec<String> = Vec::new();
    let mut last_refusal: Option<(String, StatusCode, HeaderMap, Bytes)> = None;
    let mut pending_failover: Option<(String, FailureKind)> = None;
    let mut disabled_one = false;
    // The first key to answer 401 under `hold_unauthorized`, disabled once a later key succeeds.
    let mut held: Option<(String, u16)> = None;
    let mut confirmed: Vec<String> = Vec::new();
    // The shortest wait any shaped-out key reported, for the local 429.
    let mut shaped_wait: Option<Duration> = None;
    // Time already slept on this request, across keys: the wait budget is per request.
    let mut waited = Duration::ZERO;
    for _ in 0..max_attempts {
        let Some(key_name) =
            snapshot.pick(&app.health, stream, upstream_name, &skip, scope.as_ref())
        else {
            break;
        };
        // Only count a failover once the next key is actually in hand, so a dead
        // end does not inflate the counter.
        if let Some((failed_key, kind)) = pending_failover.take() {
            app.plugins().on_event(&Event::Failover {
                stream,
                key: &failed_key,
                kind,
            });
        }
        let Some(secret) = snapshot.keys.get(&key_name) else {
            break;
        };
        // Count every selected-key attempt, so a failover or fallback cannot spend more
        // upstream calls than the stream's limit allows.
        if let Some(wait) = snapshot.rate_limit_retry_after(stream, Instant::now()) {
            tracing::warn!(stream = %stream, "stream rate limit exceeded");
            app.plugins().on_event(&Event::RateLimited { stream });
            return Ok(rate_limited_response(wait));
        }

        // Outbound shaping for this key's own upstream. A wait within its budget is
        // slept off; anything longer skips the key without touching its health.
        let estimate = (body.len() / 4) as u64;
        let mut permit: Option<Permit> = None;
        let mut shaped_out = false;
        let budget = app
            .shaper
            .get(&key_name)
            .map_or(Duration::ZERO, |(_, wait)| wait);
        let mut rounds = 0;
        loop {
            match app
                .shaper
                .acquire_within(&key_name, app.wall_now(), estimate)
            {
                Acquire::Unshaped => break,
                Acquire::Granted(granted) => {
                    permit = Some(granted);
                    break;
                }
                // The configured wait is a total, not per round.
                Acquire::Wait(wait) if rounds < 2 && waited + wait <= budget => {
                    rounds += 1;
                    waited += wait;
                    tokio::time::sleep(wait).await;
                }
                Acquire::Wait(wait) => {
                    remember_wait(&mut shaped_wait, wait);
                    shaped_out = true;
                    break;
                }
                Acquire::TooLong(wait) => {
                    remember_wait(&mut shaped_wait, wait);
                    shaped_out = true;
                    break;
                }
                Acquire::Closed(until) => {
                    remember_wait(
                        &mut shaped_wait,
                        until.duration_since(app.wall_now()).unwrap_or_default(),
                    );
                    shaped_out = true;
                    break;
                }
            }
        }
        if shaped_out {
            tracing::warn!(stream = %stream, key = %key_name, "key is shaped out, trying the next key");
            skip.push(key_name.clone());
            continue;
        }

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
                    plugin_states.on_attempt(&key_name, None, &[]);
                    return Err(forwarder_failed(key_name, plugin_states, scope));
                }
                let timed_out = failed.is_err();
                let pre_send = matches!(&failed, Ok(Err(error)) if is_pre_send(error));
                plugin_states.on_attempt(&key_name, None, &[]);
                record(
                    app,
                    snapshot,
                    stream,
                    &key_name,
                    FailureClass::Transient,
                    FailureNote {
                        scope: scope.as_ref(),
                        ..FailureNote::default()
                    },
                );
                // A POST or PATCH may already have reached the upstream, so replaying it
                // could duplicate a side effect. Only a connect failure is unambiguously
                // before the request; an explicit upstream response is handled below.
                if !is_idempotent(&parts.method) && !pre_send {
                    tracing::warn!(
                        stream = %stream,
                        key = %key_name,
                        method = %parts.method,
                        "not replaying a non-idempotent request after an ambiguous transport failure"
                    );
                    return Ok(if timed_out {
                        json_error(
                            StatusCode::GATEWAY_TIMEOUT,
                            "gateway_timeout",
                            "the upstream did not answer in time",
                        )
                    } else {
                        json_error(
                            StatusCode::BAD_GATEWAY,
                            "bad_gateway",
                            "the upstream could not be reached",
                        )
                    });
                }
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
        // Only the headers of this attempt's own upstream feed this key's bucket.
        let shaping = app.shaper.get(&key_name).map(|(bucket, _)| bucket);
        let observed = shaping.as_ref().map(|_| observe(&response.headers));
        if status.is_success() || status.is_redirection() {
            plugin_states.on_attempt(&key_name, Some(status.as_u16()), &[]);
            if let (Some(bucket), Some(observed)) = (&shaping, observed) {
                let now = app.wall_now();
                note_reported_remaining(bucket, observed, now);
                bucket.note_success(now);
                app.persist_shaper();
            }
            let had_failures = if let Ok(mut health) = app.health.lock() {
                let had = health.consecutive_failures(&key_name) > 0;
                health.record_success(&key_name);
                health.record_status(&key_name, Some(status.as_u16()));
                had
            } else {
                false
            };
            app.pressure.record(upstream_name, false, Instant::now());
            // Only write when this success changed what is on disk.
            if had_failures {
                app.persist_state();
            }
            release_held(app, snapshot, stream, held.take());
            let secrets = [secret.0.as_str(), auth_value.as_str()];
            // A decoded body is read whole below, so its permit can go as soon as that
            // read ends. Everything else streams to the client with the permit attached.
            let buffered = response.is_decoded() && !is_event_stream(&response.headers);
            let response = match settle_body(response).await {
                Ok(response) => response,
                Err(response) => return Ok(*response),
            };
            let permit = if buffered { None } else { permit };
            let accounting = Accounting {
                permit,
                pressure: snapshot
                    .config
                    .tracks_usage()
                    .then(|| Arc::clone(&app.pressure)),
            };
            return Ok(stream_response(
                plugin_states,
                &key_name,
                upstream_name,
                response,
                parts.method == Method::HEAD,
                &secrets,
                accounting,
            ));
        }

        let retry_after = response
            .headers
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let response_headers = response.headers.clone();
        let secrets = [secret.0.as_str(), auth_value.as_str()];
        let (snippet, read_error, truncated) = read_error_body(response, &secrets).await;
        if let Some(error) = &read_error {
            tracing::warn!(
                stream = %stream,
                key = %key_name,
                status = status.as_u16(),
                %error,
                "could not read the whole upstream error body; classifying by status"
            );
        }
        let mut response_headers = scrub_headers(response_headers, &secrets);
        if truncated {
            tracing::warn!(
                stream = %stream,
                key = %key_name,
                status = status.as_u16(),
                "the upstream error body is longer than the cap and is cut"
            );
            response_headers.insert(
                HeaderName::from_static("x-reiki-truncated"),
                HeaderValue::from_static("1"),
            );
        }
        plugin_states.on_attempt(&key_name, Some(status.as_u16()), &snippet);
        let text = if read_error.is_some() {
            String::new()
        } else {
            String::from_utf8_lossy(&snippet).into_owned()
        };

        let classified = if upstream.pass_errors {
            Classification {
                outcome: Outcome::PassThrough,
                quota_phrase_matched: false,
            }
        } else {
            let extra = ExtraPhrases {
                request_error: &upstream.request_error_phrases,
                quota: &upstream.quota_phrases,
                rate_limit: &upstream.rate_limit_phrases,
            };
            classify_with(
                status.as_u16(),
                retry_after.as_deref(),
                &text,
                app.wall_now(),
                &extra,
            )
        };
        let outcome = classified.outcome;
        // The first pass of a quota confirmation leaves the bucket untouched, so the
        // same key can be tried again before shaping takes it out of this request.
        let confirming = matches!(outcome, Outcome::KeyFailure(FailureClass::QuotaExhausted))
            && upstream.confirm_quota
            && status == StatusCode::TOO_MANY_REQUESTS
            && !confirmed.contains(&key_name);
        if !confirming && let (Some(bucket), Some(observed)) = (&shaping, observed) {
            let now = app.wall_now();
            note_reported_remaining(bucket, observed, now);
            // A failure that only cools one model on the key says nothing about the
            // key's own budget, so it must not pause or close the whole bucket.
            if let Outcome::KeyFailure(class) = outcome {
                let key_wide = failure_scope(
                    class,
                    Some(status.as_u16()),
                    scope.as_ref(),
                    classified.quota_phrase_matched,
                )
                .is_none();
                match class {
                    FailureClass::RateLimited(retry_after) if key_wide => {
                        bucket.note_rate_limited(now, retry_after);
                    }
                    FailureClass::QuotaExhausted if key_wide => {
                        let hint = crate::classify::parse_retry_after(retry_after.as_deref(), now);
                        let cooldown = quota_cooldown(snapshot, &key_name, class, now, hint)
                            .unwrap_or(app.policy.quota_exhausted);
                        if let Some(until) = now.checked_add(cooldown) {
                            bucket.note_window_closed(now, until);
                        }
                    }
                    _ => {}
                }
            }
            app.persist_shaper();
        }
        match outcome {
            Outcome::Success | Outcome::PassThrough => {
                if matches!(outcome, Outcome::Success) {
                    release_held(app, snapshot, stream, held.take());
                }
                return Ok(finish_buffered(
                    &mut plugin_states,
                    &key_name,
                    upstream_name,
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
                note_failure(
                    app,
                    stream,
                    &key_name,
                    FailureClass::AccessDisabled,
                    FailureNote {
                        status: Some(status.as_u16()),
                        retry_after: retry_after.as_deref(),
                        body: Some(&text),
                        secrets: &secrets,
                        ..FailureNote::default()
                    },
                );
                skip.push(key_name.clone());
                last_refusal = Some((key_name.clone(), status, response_headers, snippet));
                pending_failover = Some((key_name, FailureKind::AccessDisabled));
            }
            Outcome::RequestError => {
                // Another key may be able to afford the request, so this is a request-scoped
                // refusal: record it, skip this key, and keep the response for a dead end.
                tracing::warn!(stream = %stream, key = %key_name, status = status.as_u16(), "request needs more than this key can afford, trying the next key");
                note_request_error(
                    app,
                    stream,
                    &key_name,
                    FailureNote {
                        status: Some(status.as_u16()),
                        retry_after: retry_after.as_deref(),
                        body: Some(&text),
                        secrets: &secrets,
                        ..FailureNote::default()
                    },
                );
                skip.push(key_name.clone());
                last_refusal = Some((key_name.clone(), status, response_headers, snippet));
                pending_failover = Some((key_name, FailureKind::RequestError));
            }
            Outcome::KeyFailure(FailureClass::QuotaExhausted) if confirming => {
                // The key stays in rotation: the next pass picks it again and only a repeat is recorded.
                tracing::warn!(stream = %stream, key = %key_name, "quota 429, sending it once more before cooling the key");
                note_failure(
                    app,
                    stream,
                    &key_name,
                    FailureClass::QuotaExhausted,
                    FailureNote {
                        status: Some(status.as_u16()),
                        retry_after: retry_after.as_deref(),
                        body: Some(&text),
                        secrets: &secrets,
                        ..FailureNote::default()
                    },
                );
                confirmed.push(key_name);
                tokio::time::sleep(QUOTA_CONFIRM_DELAY).await;
            }
            Outcome::KeyFailure(FailureClass::Unauthorized)
                if disabled_one || (upstream.hold_unauthorized && held.is_some()) =>
            {
                // A second key refusing the same request points at the request, not at the keys.
                tracing::warn!(stream = %stream, key = %key_name, status = status.as_u16(), "key refused this request again, not disabling it");
                note_failure(
                    app,
                    stream,
                    &key_name,
                    FailureClass::Unauthorized,
                    FailureNote {
                        status: Some(status.as_u16()),
                        retry_after: retry_after.as_deref(),
                        body: Some(&text),
                        secrets: &secrets,
                        ..FailureNote::default()
                    },
                );
                skip.push(key_name.clone());
                last_refusal = Some((key_name.clone(), status, response_headers, snippet));
                pending_failover = Some((key_name, FailureKind::Unauthorized));
            }
            Outcome::KeyFailure(FailureClass::Unauthorized) if upstream.hold_unauthorized => {
                tracing::warn!(stream = %stream, key = %key_name, status = status.as_u16(), "key refused this request, holding its disable until a later key serves it");
                note_failure(
                    app,
                    stream,
                    &key_name,
                    FailureClass::Unauthorized,
                    FailureNote {
                        status: Some(status.as_u16()),
                        retry_after: retry_after.as_deref(),
                        body: Some(&text),
                        secrets: &secrets,
                        ..FailureNote::default()
                    },
                );
                held = Some((key_name.clone(), status.as_u16()));
                skip.push(key_name.clone());
                last_refusal = Some((key_name.clone(), status, response_headers, snippet));
                pending_failover = Some((key_name, FailureKind::Unauthorized));
            }
            Outcome::KeyFailure(class) => {
                tracing::debug!(status = status.as_u16(), body = %text.chars().take(160).collect::<String>(), "upstream error");
                disabled_one |= matches!(class, FailureClass::Unauthorized);
                let quota_hint = if class == FailureClass::QuotaExhausted {
                    crate::classify::parse_retry_after(retry_after.as_deref(), app.wall_now())
                } else {
                    None
                };
                if class == FailureClass::QuotaExhausted {
                    // What the provider says about its reset, so the cooldown can follow it.
                    tracing::warn!(
                        stream = %stream,
                        key = %key_name,
                        status = status.as_u16(),
                        retry_after = ?retry_after,
                        body = %printable_snippet(&text, 200),
                        "quota failure detail"
                    );
                }
                record(
                    app,
                    snapshot,
                    stream,
                    &key_name,
                    class,
                    FailureNote {
                        status: Some(status.as_u16()),
                        scope: scope.as_ref(),
                        quota_hint,
                        retry_after: retry_after.as_deref(),
                        body: Some(&text),
                        quota_phrase_matched: classified.quota_phrase_matched,
                        secrets: &secrets,
                        ..FailureNote::default()
                    },
                );
                // A first transient 5xx does not cool the key; skip it so the walk moves on.
                if matches!(class, FailureClass::Transient) {
                    skip.push(key_name.clone());
                    last_refusal = Some((key_name.clone(), status, response_headers, snippet));
                }
                pending_failover = Some((key_name, FailureKind::from(class)));
            }
        }
    }

    if let (Some(wait), None) = (shaped_wait, &last_refusal) {
        tracing::warn!(stream = %stream, "every key is shaped out");
        app.plugins().on_event(&Event::RateLimited { stream });
        return Ok(shaped_response(wait));
    }

    Err(Box::new(Spent {
        refusal: last_refusal,
        plugin_states,
        scope,
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
        upstream_name,
        upstream,
        parts,
        rest,
        mut plugin_states,
        faithful,
        ..
    } = fwd;
    let url = target_url(upstream, rest, parts.uri.query().map(str::to_owned));
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
            plugin_states.on_attempt(PASSTHROUGH_KEY, Some(status.as_u16()), &[]);
            let response = match settle_body(response).await {
                Ok(response) => response,
                Err(response) => return Ok(*response),
            };
            Ok(stream_response(
                plugin_states,
                PASSTHROUGH_KEY,
                upstream_name,
                response,
                parts.method == Method::HEAD,
                &[],
                Accounting::default(),
            ))
        }
        Ok(Err(err)) => {
            if upstream.forward.is_some() {
                tracing::error!(error = %err, "forwarder failed");
                plugin_states.on_attempt(PASSTHROUGH_KEY, None, &[]);
                return Ok(json_error(
                    StatusCode::BAD_GATEWAY,
                    "bad_gateway",
                    "the upstream could not be reached",
                ));
            }
            plugin_states.on_attempt(PASSTHROUGH_KEY, None, &[]);
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
                plugin_states.on_attempt(PASSTHROUGH_KEY, None, &[]);
                return Ok(json_error(
                    StatusCode::BAD_GATEWAY,
                    "bad_gateway",
                    "the upstream could not be reached",
                ));
            }
            plugin_states.on_attempt(PASSTHROUGH_KEY, None, &[]);
            Ok(json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "gateway_timeout",
                "the upstream did not answer in time",
            ))
        }
    }
}

fn target_url(upstream: &crate::config::Upstream, rest: &str, query: Option<String>) -> String {
    let mut url = format!("{}{}", upstream.base_url.trim_end_matches('/'), rest);
    if let Some(query) = query {
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

/// Disables the key whose 401 was held back, now that a later key has served the request.
fn release_held(app: &App, snapshot: &Snapshot, stream: &str, held: Option<(String, u16)>) {
    if let Some((key, status)) = held {
        record(
            app,
            snapshot,
            stream,
            &key,
            FailureClass::Unauthorized,
            FailureNote {
                status: Some(status),
                // The held 401 was already added when it was held.
                ring: false,
                ..FailureNote::default()
            },
        );
    }
}

/// Combination failures: a rate-limit and a built-in 402. A configured quota phrase on 402,
/// a quota 429, a transient, a 401 and every failure without a model stay key-scoped.
fn failure_scope(
    class: FailureClass,
    status: Option<u16>,
    scope: Option<&Scope>,
    quota_phrase_matched: bool,
) -> Option<&Scope> {
    match class {
        FailureClass::RateLimited(_) => scope,
        FailureClass::QuotaExhausted
            if status == Some(StatusCode::PAYMENT_REQUIRED.as_u16()) && !quota_phrase_matched =>
        {
            scope
        }
        _ => None,
    }
}

/// What an attempt reported about its failure.
#[derive(Clone, Copy)]
struct FailureNote<'a> {
    status: Option<u16>,
    scope: Option<&'a Scope>,
    /// The provider's own wait for a quota failure, from `Retry-After`.
    quota_hint: Option<Duration>,
    /// `Retry-After` exactly as received, kept for the ring.
    retry_after: Option<&'a str>,
    /// The upstream error body, for the ring's scrubbed snippet.
    body: Option<&'a str>,
    /// A configured quota phrase chose this class, so a 402 cools the whole key.
    quota_phrase_matched: bool,
    /// Credentials to scrub from the snippet.
    secrets: &'a [&'a str],
    /// False when the caller already added this failure to the ring.
    ring: bool,
}

impl Default for FailureNote<'_> {
    fn default() -> Self {
        Self {
            status: None,
            scope: None,
            quota_hint: None,
            retry_after: None,
            body: None,
            quota_phrase_matched: false,
            secrets: &[],
            ring: true,
        }
    }
}

fn record(
    app: &App,
    snapshot: &Snapshot,
    stream: &str,
    key: &str,
    class: FailureClass,
    note: FailureNote<'_>,
) {
    let FailureNote {
        status,
        scope,
        quota_hint,
        quota_phrase_matched,
        ..
    } = note;
    let wall_now = app.wall_now();
    let quota_cooldown = quota_cooldown(snapshot, key, class, wall_now, quota_hint);
    // The same schedule or `Retry-After` that set the wait gives an absolute reset. A quota
    // without either keeps the fixed window and has no reset to publish.
    let quota_reset_at = quota_cooldown.and_then(|wait| wall_now.checked_add(wait));
    let scoped = failure_scope(class, status, scope, quota_phrase_matched);
    let upstream = snapshot
        .config
        .keys
        .get(key)
        .map(|key| key.upstream.clone());
    let escalate_after = snapshot
        .config
        .keys
        .get(key)
        .and_then(|key| snapshot.config.upstream.get(&key.upstream))
        .map_or(DEFAULT_SCOPE_ESCALATE_AFTER, |upstream| {
            upstream.scope_escalate_after
        });
    let now = Instant::now();
    let mut counted = false;
    let mut limited = false;
    let mut ring_cooled = false;
    let mut ring_cooling = None;
    if let Ok(mut health) = app.health.lock() {
        // A reload that rotated this key's secret already reset its health; the old
        // secret's failure must not disable the new one.
        let rotated =
            app.snapshot().keys.get(key).map(|s| &s.0) != snapshot.keys.get(key).map(|s| &s.0);
        if rotated {
            tracing::debug!(stream, key, ?class, "ignoring failure from a rotated key");
            return;
        }
        let cooled = health.record_failure_scoped(
            key,
            class,
            &app.policy,
            now,
            quota_cooldown,
            scoped,
            escalate_after,
        );
        health.record_status(key, status);
        limited = status == Some(429) || cooled.is_some();
        counted = true;
        ring_cooled = cooled.is_some();
        if let Some(cooled_scope) = cooled {
            if let Some(reset_at) = quota_reset_at {
                health.note_quota_reset(key, reset_at, now);
            }
            app.plugins().on_event(&Event::Cooldown {
                key,
                kind: FailureKind::from(class),
                scope: cooled_scope,
            });
        }
        let cooling_secs = health.cooling_remaining(key, now).map(|d| d.as_secs());
        ring_cooling = cooling_secs;
        tracing::warn!(
            stream,
            key,
            ?class,
            ?status,
            cooled = cooled.is_some(),
            ?cooling_secs,
            "upstream key failed"
        );
    }
    if counted && let Some(upstream) = upstream {
        app.pressure.record(&upstream, limited, now);
    }
    record_ring(
        app,
        stream,
        key,
        class.name(),
        note,
        ring_cooled,
        ring_cooling,
    );
    app.persist_state();
}

/// Adds a failure to the ring without touching key health, for branches that deliberately do
/// not cool or disable the key.
fn note_failure(app: &App, stream: &str, key: &str, class: FailureClass, note: FailureNote<'_>) {
    if !note.ring {
        return;
    }
    let cooling = app
        .health
        .try_lock()
        .ok()
        .and_then(|health| health.cooling_remaining(key, Instant::now()))
        .map(|left| left.as_secs());
    record_ring(app, stream, key, class.name(), note, false, cooling);
}

/// Adds a request error to the ring under its own label. Unlike [`note_failure`], this uses a
/// label that is not a [`FailureClass`], because the key is not at fault.
fn note_request_error(app: &App, stream: &str, key: &str, note: FailureNote<'_>) {
    if !note.ring {
        return;
    }
    let cooling = app
        .health
        .try_lock()
        .ok()
        .and_then(|health| health.cooling_remaining(key, Instant::now()))
        .map(|left| left.as_secs());
    record_ring(
        app,
        stream,
        key,
        FailureKind::RequestError.label(),
        note,
        false,
        cooling,
    );
}

fn record_ring(
    app: &App,
    stream: &str,
    key: &str,
    class: &str,
    note: FailureNote<'_>,
    cooled: bool,
    cooling_seconds: Option<u64>,
) {
    if !note.ring {
        return;
    }
    let retry_after = note.retry_after.map(|value| {
        let clean = crate::scrub::scrub_text(value, note.secrets);
        printable_snippet(&clean, crate::failures::MAX_RETRY_AFTER_CHARS)
    });
    let snippet = note.body.map_or_else(String::new, |body| {
        let clean = crate::scrub::scrub_text(body, note.secrets);
        if crate::scrub::contains_key_material(&clean, note.secrets) {
            WITHHELD_NOTE.to_owned()
        } else {
            printable_snippet(&clean, crate::failures::MAX_NOTE_CHARS)
        }
    });
    let time = app
        .wall_now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    app.failures().record(crate::failures::FailureRecord {
        time,
        stream: stream.to_owned(),
        key: key.to_owned(),
        status: note.status,
        class: class.to_owned(),
        retry_after,
        cooled,
        cooling_seconds,
        note: snippet,
    });
}

/// The cooldown for a quota failure when the key names a reset schedule or the provider sent a
/// `Retry-After`; `None` otherwise, so the fixed policy window applies.
fn quota_cooldown(
    snapshot: &Snapshot,
    key: &str,
    class: FailureClass,
    now: SystemTime,
    hint: Option<Duration>,
) -> Option<Duration> {
    if class != FailureClass::QuotaExhausted {
        return None;
    }
    quota_window(
        snapshot
            .config
            .quota_reset(key)
            .and_then(|reset| reset.next_after(now)),
        hint,
    )
}

/// A configured reset schedule wins over the provider's hint, which wins over the policy window.
fn quota_window(schedule: Option<Duration>, hint: Option<Duration>) -> Option<Duration> {
    schedule.or(hint.filter(|wait| !wait.is_zero()))
}

/// Printable, bounded text from an upstream body for a log line.
pub(crate) fn printable_snippet(text: &str, max: usize) -> String {
    text.chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn exhausted(
    snapshot: &Snapshot,
    health: &Mutex<KeyState>,
    stream: &str,
    upstream: &str,
    scope: Option<&Scope>,
) -> Response {
    let mut response = json_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "reiki_exhausted",
        "every key in this stream's chain is unavailable",
    );
    if let Some(wait) = snapshot.soonest_recovery(health, stream, upstream, scope) {
        let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        if let Ok(value) = HeaderValue::from_str(&secs.max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

/// Methods RFC 9110 defines as idempotent, so replaying them after an ambiguous
/// transport failure cannot duplicate a side effect.
fn is_idempotent(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS | Method::TRACE
    )
}

/// True for a transport failure that happened while connecting, before any request
/// bytes could be sent. The transports map that case to `ConnectionRefused`.
fn is_pre_send(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ConnectionRefused
}

async fn read_limited(
    mut response: UpstreamResponse,
    extra: usize,
) -> (Bytes, Option<std::io::Error>) {
    // One byte past the cap and the longest secret form, so the caller can tell a body that
    // ends at the cap from one that goes on, and can scrub a secret that straddles the cap.
    let limit = MAX_ERROR_BODY.saturating_add(extra).saturating_add(1);
    let mut collected = Vec::new();
    while let Some(chunk) = response.chunk().await {
        match chunk {
            Ok(chunk) => {
                let room = limit.saturating_sub(collected.len());
                collected.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if collected.len() >= limit {
                    break;
                }
            }
            Err(error) => return (Bytes::from(collected), Some(error)),
        }
    }
    (Bytes::from(collected), None)
}

/// Reads an upstream error body, scrubs the secrets from it and only then cuts it to
/// `MAX_ERROR_BODY`. Cutting first would leave the head of a secret that straddles the cut.
/// The flag says the body went on past the cut.
async fn read_error_body(
    response: UpstreamResponse,
    secrets: &[&str],
) -> (Bytes, Option<std::io::Error>, bool) {
    let longest = crate::scrub::forms(secrets).first().map_or(0, Vec::len);
    let (body, error) = read_limited(response, longest).await;
    let body = scrub(body, secrets);
    if body.len() > MAX_ERROR_BODY {
        (body.slice(..MAX_ERROR_BODY), error, true)
    } else {
        (body, error, false)
    }
}

/// True for a response the client consumes incrementally, so it must not be held back.
fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
}

/// Reads a decoded, non-streaming body whole, so an expansion past `max_decoded_bytes`
/// becomes a clean 502 before the client sees any of it. SSE and plain bodies keep streaming.
async fn settle_body(response: UpstreamResponse) -> Result<UpstreamResponse, Box<Response>> {
    if !response.is_decoded() || is_event_stream(&response.headers) {
        return Ok(response);
    }
    response.buffer().await.map_err(|error| {
        Box::new(if crate::upstream::is_body_limit_error(&error) {
            tracing::warn!("decoded response body exceeded the configured size limit");
            json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                "the upstream response body exceeds the size limit",
            )
        } else if error.kind() == std::io::ErrorKind::TimedOut {
            tracing::warn!(%error, "the upstream response body timed out");
            json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "gateway_timeout",
                "the upstream response body timed out",
            )
        } else {
            tracing::warn!(%error, "could not read the decoded response body");
            json_error(
                StatusCode::BAD_GATEWAY,
                "bad_gateway",
                "the upstream response body could not be read",
            )
        })
    })
}

/// Headers that identify the account behind a shared key, or set state in the client.
fn is_account_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "openai-organization"
            | "openai-project"
            | "anthropic-organization-id"
            | "set-cookie"
            | "set-cookie2"
    )
}

/// Replaces a key (or its formatted credential) an upstream echoed back in text relayed to a client.
fn scrub(bytes: Bytes, secrets: &[&str]) -> Bytes {
    let mut data = bytes;
    for needle in crate::scrub::forms(secrets) {
        let needle = needle.as_slice();
        if !data.windows(needle.len()).any(|w| w == needle) {
            continue;
        }
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i..].starts_with(needle) {
                out.extend_from_slice(b"[redacted]");
                i += needle.len();
            } else {
                out.push(data[i]);
                i += 1;
            }
        }
        data = Bytes::from(out);
    }
    data
}

fn scrub_headers(headers: HeaderMap, secrets: &[&str]) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in &headers {
        let cleaned = scrub(Bytes::copy_from_slice(value.as_bytes()), secrets);
        let value = HeaderValue::from_bytes(&cleaned).unwrap_or_else(|_| value.clone());
        out.append(name.clone(), value);
    }
    out
}

fn copy_response_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        if !skip_header(name) && !is_account_header(name) {
            to.append(name.clone(), value.clone());
        }
    }
}

/// What a streamed body settles when it ends: the shaping permit, whose estimate the measured
/// usage replaces, and the counter that takes the measured tokens.
#[derive(Default)]
struct Accounting {
    permit: Option<Permit>,
    pressure: Option<Arc<PressureCounter>>,
}

/// Holds a shaping permit for as long as its body is streamed. Dropping the body
/// (including a client disconnect) releases the key's concurrency slot.
struct Permitted<S> {
    inner: S,
    permit: Option<Permit>,
    usage: Option<Usage>,
    /// The counter and the upstream its tokens are attributed to.
    tokens: Option<(Arc<PressureCounter>, String)>,
}

impl<S> Permitted<S> {
    /// Settles with the usage seen so far. Without usage the permit is left to drop, keeping
    /// the estimate, and no tokens are counted.
    fn finish(&mut self) {
        let Some(mut usage) = self.usage.take() else {
            return;
        };
        let Some(actual) = usage.total() else {
            return;
        };
        if let Some(permit) = self.permit.take() {
            permit.complete(actual);
        }
        if let Some((pressure, upstream)) = self.tokens.take() {
            pressure.record_tokens(&upstream, actual, Instant::now());
        }
    }
}

impl<S> Drop for Permitted<S> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl<S> Stream for Permitted<S>
where
    S: Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                if let Some(usage) = &mut this.usage {
                    usage.feed(&chunk);
                }
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                this.finish();
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                this.finish();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

fn stream_response(
    plugin_states: PluginSession,
    key: &str,
    upstream: &str,
    response: UpstreamResponse,
    is_head: bool,
    secrets: &[&str],
    accounting: Accounting,
) -> Response {
    let mut status = response.status;
    let mut headers = HeaderMap::new();
    // A coding that remains in the headers is one Reiki did not decode: a list such as
    // `gzip, br` or an unknown name. The body's echoed key cannot be scrubbed in that form, so
    // refuse it instead of handing the raw bytes to a plugin or the client. Bodyless responses
    // have nothing to scrub and keep their headers.
    if !secrets.is_empty()
        && is_encoded(&response.headers)
        && !is_head
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED
    {
        tracing::warn!(%upstream, "upstream left a content coding Reiki cannot decode; refusing an unscrubbed body");
        return json_error(
            StatusCode::BAD_GATEWAY,
            "bad_gateway",
            "the upstream response uses a content coding Reiki cannot decode",
        );
    }
    // An upstream that echoes the key back must not hand it to a plugin or the client.
    let echoed_headers = if secrets.is_empty() {
        None
    } else {
        Some(scrub_headers(response.headers.clone(), secrets))
    };
    copy_response_headers(
        echoed_headers.as_ref().unwrap_or(&response.headers),
        &mut headers,
    );
    // Pass the length through when the upstream gave one, so non-chunked bodies stay non-chunked.
    if let Some(length) = response.headers.get(header::CONTENT_LENGTH) {
        headers.insert(header::CONTENT_LENGTH, length.clone());
    }
    let scrubber = if is_encoded(&response.headers) {
        None
    } else {
        crate::scrub::StreamScrubber::new(secrets)
    };
    // A compressed body is not the upstream's bytes, so its usage cannot be read here.
    let watch = !response.is_decoded() && !is_encoded(&response.headers);
    let Accounting { permit, pressure } = accounting;
    let usage = (watch && (permit.is_some() || pressure.is_some()))
        .then(|| Usage::new(is_event_stream(&response.headers)));
    let tokens = pressure.map(|pressure| (pressure, upstream.to_owned()));
    let stream = |response: UpstreamResponse| -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
        match scrubber {
            Some(scrubber) => Box::pin(crate::scrub::ScrubbedStream::new(response.into_stream(), scrubber)),
            None => response.into_stream(),
        }
    };
    let body = if !plugin_states.needs_response_hooks() {
        Body::from_stream(Permitted {
            inner: stream(response),
            permit,
            usage,
            tokens,
        })
    } else {
        let mut session = plugin_states;
        // A plugin that changes the body length must drop content-length here.
        session.on_response_head(key, upstream, &mut status, &mut headers);
        if session.failed() {
            return json_error(
                PLUGIN_FAILURE_STATUS,
                "plugin_failed",
                PLUGIN_FAILURE_MESSAGE,
            );
        }
        // A plugin may change the body length, and a known length would also stop
        // the final `is_last` call from being made. These responses carry no body, so
        // their length is only metadata.
        let bodyless =
            is_head || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED;
        if !bodyless && !session.is_empty() {
            headers.remove(header::CONTENT_LENGTH);
        }
        Body::from_stream(Permitted {
            inner: PluginBody {
                session,
                inner: stream(response),
                done: false,
            },
            permit,
            usage,
            tokens,
        })
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// Runs the response hooks over a body that arrived in one piece.
fn finish_buffered(
    session: &mut PluginSession,
    key: &str,
    upstream: &str,
    status: StatusCode,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if !session.needs_response_hooks() {
        return buffered_response(status, headers, body);
    }
    let mut status = status;
    let mut headers = headers.clone();
    session.on_response_head(key, upstream, &mut status, &mut headers);
    let mut body = body;
    session.on_response_chunk(&mut body, true);
    if session.failed() {
        return json_error(
            PLUGIN_FAILURE_STATUS,
            "plugin_failed",
            PLUGIN_FAILURE_MESSAGE,
        );
    }
    buffered_response(status, &headers, body)
}

/// Streams the upstream body through the plugin chain a chunk at a time.
///
/// Each chunk is released as soon as its plugins have seen it, so a placeholder
/// split across chunks only delays the chunks it spans. The final empty poll
/// gives plugins the `is_last` call to release anything they held back.
struct PluginBody {
    session: PluginSession,
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    done: bool,
}

impl Drop for PluginBody {
    fn drop(&mut self) {
        // A client disconnect or a cancelled request drops the body before EOF. Give
        // plugins the same final call they would get from a normal end, so counters and
        // buffers do not leak; any bytes they would release are no longer deliverable.
        if !self.done {
            self.done = true;
            let mut tail = Bytes::new();
            self.session.on_response_chunk(&mut tail, true);
        }
    }
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
                    this.session.on_response_chunk(&mut tail, true);
                    if this.session.failed() {
                        return Poll::Ready(Some(Err(std::io::Error::other(
                            PLUGIN_FAILURE_MESSAGE,
                        ))));
                    }
                    return if tail.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(Ok(tail)))
                    };
                }
                Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    let mut tail = Bytes::new();
                    this.session.on_response_chunk(&mut tail, true);
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Some(Ok(mut chunk))) => {
                    this.session.on_response_chunk(&mut chunk, false);
                    if this.session.failed() {
                        // Headers are already on the wire, so the only safe response is to cut
                        // the body short rather than relay bytes the plugin could not process.
                        this.done = true;
                        return Poll::Ready(Some(Err(std::io::Error::other(
                            PLUGIN_FAILURE_MESSAGE,
                        ))));
                    }
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
    #[test]
    fn identity_content_coding_is_plain_and_anything_else_is_not() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
            headers
        };
        assert!(!is_encoded(&HeaderMap::new()));
        assert!(!is_encoded(&with("identity")));
        assert!(!is_encoded(&with(" Identity , identity ")));
        assert!(is_encoded(&with("gzip")));
        assert!(is_encoded(&with("identity, gzip")));
        assert!(is_encoded(&with("x-unknown")));
    }

    use super::*;
    use crate::plugins::Registration;
    use reiki_plugin::{ChunkCtx, Plugin};

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
    fn only_idempotent_methods_may_replay_an_ambiguous_failure() {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
            Method::TRACE,
        ] {
            assert!(is_idempotent(&method), "{method}");
        }
        for method in [Method::POST, Method::PATCH] {
            assert!(!is_idempotent(&method), "{method}");
        }
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(is_pre_send(&refused));
        let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert!(!is_pre_send(&timed_out));
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
        assert!(
            !out.values()
                .any(|v| v.to_str().is_ok_and(|s| s.contains("caller-token")))
        );
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
    fn retry_after_reflects_only_the_tried_upstreams_keys() {
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://zen.example/v1\"\n\
             [upstream.backup]\nbase_url = \"https://backup.example/v1\"\n\
             [keys.zen-a]\nupstream = \"zen\"\nfile = \"/unused/zen-a\"\n\
             [keys.backup-a]\nupstream = \"backup\"\nfile = \"/unused/backup-a\"\n\
             [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"zen-a\", \"backup-a\"]\n",
            crate::auth::sha256_hex("tok")
        );
        let snapshot = Snapshot::new(Config::parse(&text).unwrap(), HashMap::new());
        let health = Mutex::new(KeyState::default());
        {
            let mut health = health.lock().unwrap();
            health.record_failure(
                "zen-a",
                FailureClass::RateLimited(Some(Duration::from_secs(3600))),
                &Policy::default(),
                Instant::now(),
            );
        }

        let response = exhausted(&snapshot, &health, "s", "zen", None);
        assert_eq!(
            response.headers()[header::RETRY_AFTER],
            "3600",
            "the healthy key of another upstream must not shorten the wait"
        );
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
            snapshot.pick(&health, "s", "zen", &[], None),
            Some("a".to_owned())
        );
        assert!(
            snapshot
                .soonest_recovery(&health, "s", "zen", None)
                .is_some()
        );
    }

    #[test]
    fn a_poisoned_rate_limiter_lock_still_enforces_the_limit() {
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
             [streams.s]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\nrate_limit_per_minute = 1\n",
            crate::auth::sha256_hex("tok")
        );
        let snapshot = Arc::new(Snapshot::new(Config::parse(&text).unwrap(), HashMap::new()));
        let poisoner = Arc::clone(&snapshot);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.limiters.get("s").unwrap().lock().unwrap();
            panic!("poison the limiter lock");
        })
        .join();
        assert!(snapshot.limiters.get("s").unwrap().is_poisoned());

        let now = Instant::now();
        assert!(snapshot.rate_limit_retry_after("s", now).is_none());
        assert!(
            snapshot.rate_limit_retry_after("s", now).is_some(),
            "a poisoned lock must not fail open"
        );
    }

    #[test]
    fn dot_segments_and_encoded_separators_are_path_tricks() {
        for path in [
            "/up/../admin",
            "/up/./x",
            "/up/%2e%2e/admin",
            "/up/%2E/x",
            "/up/.%2e/x",
            "/up/a%2fb",
            "/up/a%5Cb",
            "/up\\x",
            "/up/%252e%252e/admin",
            "/up/%252Fadmin",
            "/up/%255cadmin",
            "/up/%25252e%25252e/admin",
            "/up/%",
            "/up/%zz",
        ] {
            assert!(has_path_trick(path), "{path}");
        }
        for path in [
            "/up/v1/chat",
            "/up/a..b/c",
            "/up/.hidden",
            "/up/..x",
            "/",
            "/up/a%2520b",
        ] {
            assert!(!has_path_trick(path), "{path}");
        }
    }

    #[test]
    fn an_echoed_key_is_scrubbed_from_relayed_text() {
        let body = Bytes::from_static(b"bad header: Bearer sk-secret-1234 / sk-secret-1234 end");
        let out = scrub(body, &["sk-secret-1234", "Bearer sk-secret-1234"]);
        assert_eq!(
            out,
            Bytes::from_static(b"bad header: [redacted] / [redacted] end")
        );
        // Short values are left alone, they are too likely to be ordinary text.
        assert_eq!(scrub(Bytes::from_static(b"a b c"), &["b"]), "a b c");
        let mut headers = HeaderMap::new();
        headers.insert("x-echo", HeaderValue::from_static("sk-secret-1234"));
        let cleaned = scrub_headers(headers, &["sk-secret-1234"]);
        assert_eq!(cleaned["x-echo"], "[redacted]");
    }

    #[test]
    fn encoded_forms_of_an_echoed_key_are_scrubbed_from_text_and_headers() {
        let secrets = ["sk-a/b+c\"d-1234"];
        let body = Bytes::from_static(
            b"{\"k\":\"sk-a\\/b+c\\\"d-1234\"} sk-a%2Fb%2Bc%22d-1234 c2stYS9iK2MiZC0xMjM0 ok",
        );
        let out = scrub(body, &secrets);
        assert_eq!(
            out,
            Bytes::from_static(b"{\"k\":\"[redacted]\"} [redacted] [redacted] ok")
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-echo", HeaderValue::from_static("sk-a%2Fb%2Bc%22d-1234"));
        assert_eq!(scrub_headers(headers, &secrets)["x-echo"], "[redacted]");
    }

    #[tokio::test]
    async fn an_undecodable_coding_is_refused_when_a_key_could_be_echoed() {
        let secret = "sk-secret-1234";
        for coding in ["gzip, br", "x-unknown"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_ENCODING,
                HeaderValue::from_str(coding).unwrap(),
            );
            let response = UpstreamResponse::from_bytes(
                StatusCode::OK,
                headers,
                axum::http::Version::HTTP_11,
                Bytes::from(format!("echo {secret}").into_bytes()),
            );
            let out = stream_response(
                PluginSession::default(),
                "k",
                "up",
                response,
                false,
                &[secret],
                Accounting::default(),
            );
            assert_eq!(out.status(), StatusCode::BAD_GATEWAY, "{coding}");
            let body = to_bytes(out.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains("content coding"), "{coding}: {body}");
            assert!(
                !body.contains(secret),
                "{coding}: the key must not be relayed"
            );
        }
    }

    #[tokio::test]
    async fn an_undecodable_coding_still_passes_through_without_a_key() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_static("gzip, br"),
        );
        let response = UpstreamResponse::from_bytes(
            StatusCode::OK,
            headers,
            axum::http::Version::HTTP_11,
            Bytes::from_static(b"raw bytes"),
        );
        let out = stream_response(
            PluginSession::default(),
            "k",
            "up",
            response,
            false,
            &[],
            Accounting::default(),
        );
        assert_eq!(out.status(), StatusCode::OK);
        assert_eq!(
            out.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip, br"
        );
        assert_eq!(
            to_bytes(out.into_body(), usize::MAX).await.unwrap(),
            Bytes::from_static(b"raw bytes")
        );
    }

    #[test]
    fn account_identifying_headers_are_not_relayed() {
        let mut from = HeaderMap::new();
        from.insert("openai-organization", HeaderValue::from_static("org-1"));
        from.insert("set-cookie", HeaderValue::from_static("a=b"));
        from.insert("x-ratelimit-remaining", HeaderValue::from_static("5"));
        let mut to = HeaderMap::new();
        copy_response_headers(&from, &mut to);
        assert!(to.get("openai-organization").is_none());
        assert!(to.get("set-cookie").is_none());
        assert!(to.get("x-ratelimit-remaining").is_some());
    }

    #[test]
    fn request_done_emits_499_when_the_request_task_is_dropped() {
        static EVENTS: Mutex<Vec<(String, String, u16)>> = Mutex::new(Vec::new());

        struct Recorder;
        impl reiki_plugin::Plugin for Recorder {
            fn name(&self) -> &str {
                "recorder"
            }
            fn on_event(&self, event: &Event<'_>) {
                if let Event::RequestDone {
                    stream,
                    upstream,
                    status,
                } = event
                {
                    EVENTS.lock().unwrap().push((
                        (*stream).to_owned(),
                        (*upstream).to_owned(),
                        *status,
                    ));
                }
            }
        }
        fn chain() -> PluginChain {
            let registry = vec![crate::plugins::Registration {
                name: "recorder",
                build: |_| Ok(Box::new(Recorder) as Box<dyn reiki_plugin::Plugin>),
            }];
            let configured: std::collections::BTreeMap<String, toml::Table> =
                toml::from_str("[recorder]\n").unwrap();
            PluginChain::build(&configured, &registry).unwrap()
        }

        // A task dropped before it finishes reports 499, exactly once.
        let dropped = RequestDone::new(Arc::new(chain()), "client-a", "zen");
        drop(dropped);
        assert_eq!(
            *EVENTS.lock().unwrap(),
            [("client-a".into(), "zen".into(), 499)]
        );

        // A finished request keeps its own status; the drop must not add another event.
        let finished = RequestDone::new(Arc::new(chain()), "client-a", "zen");
        finished.finish(200);
        assert_eq!(
            *EVENTS.lock().unwrap(),
            [
                ("client-a".to_owned(), "zen".to_owned(), 499),
                ("client-a".to_owned(), "zen".to_owned(), 200),
            ]
        );
    }

    #[test]
    fn a_dropped_response_body_still_gives_plugins_their_final_chunk_call() {
        static CHUNKS: Mutex<Vec<(Vec<u8>, bool)>> = Mutex::new(Vec::new());

        struct ChunkRecorder;
        impl Plugin for ChunkRecorder {
            fn name(&self) -> &str {
                "chunk-recorder"
            }
            fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
                CHUNKS
                    .lock()
                    .unwrap()
                    .push((ctx.chunk.to_vec(), ctx.is_last));
            }
        }
        let registry = vec![Registration {
            name: "chunk-recorder",
            build: |_| Ok(Box::new(ChunkRecorder) as Box<dyn Plugin>),
        }];
        let chain = Arc::new(
            PluginChain::build(&toml::from_str("[chunk-recorder]\n").unwrap(), &registry).unwrap(),
        );
        let mut session = chain.session();
        let method = Method::GET;
        let meta = RequestMeta {
            id: "",
            stream: "client-a",
            upstream: "zen",
            method: &method,
            path: "/zen/x",
        };
        let mut headers = HeaderMap::new();
        let mut body = Bytes::new();
        session.on_request(&meta, &mut headers, &mut body).unwrap();

        // A stream that has more to give, like one cut short by a client disconnect.
        struct Stuck;
        impl futures_core::Stream for Stuck {
            type Item = Result<Bytes, std::io::Error>;
            fn poll_next(
                self: Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                std::task::Poll::Ready(Some(Ok(Bytes::from_static(b"partial"))))
            }
        }
        let plugin_body = PluginBody {
            session,
            inner: Box::pin(Stuck),
            done: false,
        };
        drop(plugin_body);
        let seen = CHUNKS.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            &[(Vec::new(), true)],
            "the disconnect must balance with exactly one final call"
        );
    }

    #[tokio::test]
    async fn a_body_that_errors_midway_still_gets_one_final_chunk_call() {
        use futures_util::StreamExt;
        static CHUNKS: Mutex<Vec<bool>> = Mutex::new(Vec::new());

        struct LastRecorder;
        impl Plugin for LastRecorder {
            fn name(&self) -> &str {
                "last-recorder"
            }
            fn on_response_chunk(&self, ctx: &mut ChunkCtx<'_>) {
                CHUNKS.lock().unwrap().push(ctx.is_last);
            }
        }
        let registry = vec![Registration {
            name: "last-recorder",
            build: |_| Ok(Box::new(LastRecorder) as Box<dyn Plugin>),
        }];
        let chain = Arc::new(
            PluginChain::build(&toml::from_str("[last-recorder]\n").unwrap(), &registry).unwrap(),
        );
        let mut session = chain.session();
        let method = Method::GET;
        let meta = RequestMeta {
            id: "",
            stream: "client-a",
            upstream: "zen",
            method: &method,
            path: "/zen/x",
        };
        session
            .on_request(&meta, &mut HeaderMap::new(), &mut Bytes::new())
            .unwrap();
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"part")),
            Err(std::io::Error::other("reset")),
        ];
        let mut body = PluginBody {
            session,
            inner: Box::pin(futures_util::stream::iter(chunks)),
            done: false,
        };
        assert!(body.next().await.unwrap().is_ok());
        assert!(body.next().await.unwrap().is_err());
        drop(body);
        assert_eq!(*CHUNKS.lock().unwrap(), [false, true]);
    }

    /// A session whose response hook panics, as a fail-closed plugin would.
    fn panicking_response_session() -> PluginSession {
        struct PanicResponse;
        impl Plugin for PanicResponse {
            fn name(&self) -> &str {
                "panic_response"
            }
            fn fails_closed(&self) -> bool {
                true
            }
            fn needs_response_hooks(&self) -> bool {
                true
            }
            fn on_response_head(&self, _ctx: &mut reiki_plugin::ResponseHeadCtx<'_>) {
                panic!("boom");
            }
            fn on_response_chunk(&self, _ctx: &mut ChunkCtx<'_>) {
                panic!("boom");
            }
        }
        let registry = vec![Registration {
            name: "panic_response",
            build: |_| Ok(Box::new(PanicResponse) as Box<dyn Plugin>),
        }];
        let chain = Arc::new(
            PluginChain::build(&toml::from_str("[panic_response]\n").unwrap(), &registry).unwrap(),
        );
        let mut session = chain.session();
        let method = Method::GET;
        let meta = RequestMeta {
            id: "test",
            stream: "client-a",
            upstream: "zen",
            method: &method,
            path: "/zen/x",
        };
        session
            .on_request(&meta, &mut HeaderMap::new(), &mut Bytes::new())
            .unwrap();
        session
    }

    #[test]
    fn a_panicking_response_head_refuses_a_streamed_response() {
        let response = UpstreamResponse::from_stream(
            StatusCode::OK,
            HeaderMap::new(),
            axum::http::Version::HTTP_11,
            futures_util::stream::iter(Vec::<Result<Bytes, std::io::Error>>::new()),
        );
        let out = stream_response(
            panicking_response_session(),
            "k",
            "zen",
            response,
            false,
            &[],
            Accounting::default(),
        );
        assert_eq!(out.status(), PLUGIN_FAILURE_STATUS);
    }

    #[test]
    fn a_panicking_response_hook_refuses_a_buffered_response() {
        let mut session = panicking_response_session();
        let response = finish_buffered(
            &mut session,
            "k",
            "zen",
            StatusCode::OK,
            &HeaderMap::new(),
            Bytes::from_static(b"unprocessed secret"),
        );
        assert_eq!(response.status(), PLUGIN_FAILURE_STATUS);
    }

    #[tokio::test]
    async fn a_panicking_chunk_hook_cuts_the_stream_short() {
        use futures_util::StreamExt;

        let mut body = PluginBody {
            session: panicking_response_session(),
            inner: Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from_static(
                b"unprocessed secret",
            ))])),
            done: false,
        };
        let error = body.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("fail-closed plugin"), "{error}");
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn a_body_read_error_is_reported_with_the_bytes_read_so_far() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"quota")),
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "body stalled",
            )),
        ];
        let response = UpstreamResponse::from_stream(
            StatusCode::TOO_MANY_REQUESTS,
            HeaderMap::new(),
            axum::http::Version::HTTP_11,
            futures_util::stream::iter(chunks),
        );
        let (body, error) = read_limited(response, 0).await;
        assert_eq!(&body[..], b"quota");
        assert!(error.is_some(), "a read error must not look like EOF");
    }

    fn error_response(chunks: Vec<Vec<u8>>) -> UpstreamResponse {
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            chunks.into_iter().map(|c| Ok(Bytes::from(c))).collect();
        UpstreamResponse::from_stream(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            axum::http::Version::HTTP_11,
            futures_util::stream::iter(chunks),
        )
    }

    #[tokio::test]
    async fn a_secret_straddling_the_cap_is_scrubbed_before_the_cut() {
        let key = "sk-straddle-0123456789abcdef0123456789";
        for offset in [3, 10, key.len() - 1] {
            // The key starts `offset` bytes before the cap, so only its head fits.
            let mut body = vec![b'x'; MAX_ERROR_BODY - offset];
            body.extend_from_slice(key.as_bytes());
            body.extend(std::iter::repeat_n(b'y', 100));
            let (cut, error, truncated) = read_error_body(error_response(vec![body]), &[key]).await;
            assert!(error.is_none());
            assert!(truncated);
            assert!(cut.len() <= MAX_ERROR_BODY);
            assert!(
                !String::from_utf8_lossy(&cut).contains("sk-"),
                "offset {offset}: the head of the key reached the client"
            );
        }
    }

    #[tokio::test]
    async fn a_secret_straddling_the_cap_across_chunks_is_scrubbed() {
        let key = "sk-chunked-0123456789abcdef0123456789";
        let mut first = vec![b'x'; MAX_ERROR_BODY - 8];
        first.extend_from_slice(&key.as_bytes()[..8]);
        let second = key.as_bytes()[8..].to_vec();
        let (cut, _, truncated) =
            read_error_body(error_response(vec![first, second, vec![b'z'; 50]]), &[key]).await;
        assert!(truncated);
        assert!(!String::from_utf8_lossy(&cut).contains("sk-"));
    }

    #[tokio::test]
    async fn a_body_ending_exactly_at_the_cap_is_not_flagged_and_a_longer_one_is() {
        let exact = vec![b'a'; MAX_ERROR_BODY];
        let (body, _, truncated) = read_error_body(error_response(vec![exact]), &[]).await;
        assert_eq!(body.len(), MAX_ERROR_BODY);
        assert!(!truncated);
        let longer = vec![b'a'; MAX_ERROR_BODY + 1];
        let (body, _, truncated) = read_error_body(error_response(vec![longer]), &[]).await;
        assert_eq!(body.len(), MAX_ERROR_BODY);
        assert!(truncated);
        let short = b"small".to_vec();
        let (body, _, truncated) = read_error_body(error_response(vec![short]), &[]).await;
        assert_eq!(&body[..], b"small");
        assert!(!truncated);
    }

    #[test]
    #[cfg(unix)]
    fn load_keys_refuses_a_loosely_permissioned_key_unless_strict_checks_are_off() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "reiki-perm-key-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k");
        std::fs::write(&path, "eight-by\n").unwrap();
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://example.com\"\n\
             [keys.k]\nupstream = \"zen\"\nfile = \"{}\"\n",
            path.display()
        );

        let config = Config::parse(&text).unwrap();
        assert!(config.strict_key_permissions);
        for mode in [0o644, 0o620, 0o622, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let err = load_keys(&config).unwrap_err().to_string();
            assert!(
                err.contains(&format!("{mode:04o}")) && err.contains("chmod 600"),
                "mode {mode:04o} must be refused: {err}"
            );

            let relaxed =
                Config::parse(&format!("strict_key_permissions = false\n{text}")).unwrap();
            assert!(load_keys(&relaxed).is_ok());
        }

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_keys(&config).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_keys_refuses_a_key_too_short_to_be_scrubbed() {
        let dir = std::env::temp_dir().join(format!(
            "reiki-short-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::keys::write_key(&dir.join("k"), "abc\n");
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://example.com\"\n\
             [keys.k]\nupstream = \"zen\"\nfile = \"{}\"\n",
            dir.join("k").display()
        );
        let config = Config::parse(&text).unwrap();
        let err = load_keys(&config).unwrap_err().to_string();
        assert!(
            err.contains("8"),
            "the error names the minimum length: {err}"
        );
        assert!(err.contains("k"), "the error names the key: {err}");
        // A key of exactly the minimum length loads.
        crate::keys::write_key(&dir.join("k"), "eight-by\n");
        let config = Config::parse(&text).unwrap();
        assert!(load_keys(&config).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_keys_refuses_a_key_too_long_to_scrub_with_a_bounded_form_list() {
        let dir = std::env::temp_dir().join(format!(
            "reiki-long-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://example.com\"\n\
             [keys.k]\nupstream = \"zen\"\nfile = \"{}\"\n",
            dir.join("k").display()
        );
        std::fs::write(dir.join("k"), "x".repeat(10_000)).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("k"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let config = Config::parse(&text).unwrap();
        let err = load_keys(&config).unwrap_err().to_string();
        assert!(err.contains("10000"), "the error names the length: {err}");
        assert!(err.contains("4096"), "the error names the cap: {err}");
        assert!(err.contains('k'), "the error names the key: {err}");
        // A key at the cap loads.
        std::fs::write(dir.join("k"), "x".repeat(crate::scrub::MAX_SECRET)).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("k"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let config = Config::parse(&text).unwrap();
        assert!(load_keys(&config).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_quota_window_prefers_the_schedule_then_the_providers_hint() {
        let schedule = Some(Duration::from_secs(600));
        let hint = Some(Duration::from_secs(90));
        assert_eq!(quota_window(schedule, hint), schedule);
        assert_eq!(quota_window(None, hint), hint);
        assert_eq!(quota_window(None, Some(Duration::ZERO)), None);
        assert_eq!(quota_window(None, None), None);
    }

    #[test]
    fn a_logged_snippet_is_bounded_and_has_no_control_characters() {
        let text = "a\u{1b}[31mb\nc".repeat(100);
        let out = printable_snippet(&text, 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.chars().all(|c| !c.is_control()));
    }

    #[test]
    fn a_ring_snippet_scrubs_the_key_and_strips_control_characters() {
        let app = App::with_header_timeout(
            Config::parse("").unwrap(),
            HashMap::new(),
            Policy::default(),
            Duration::from_secs(1),
        );
        let secret_text = ["sk-ring", "-secret-0123456789"].concat();
        let secret = secret_text.as_str();
        let body = format!("bad {secret}\u{1b}[31m\n{}", "x".repeat(300));
        record_ring(
            &app,
            "s",
            "a",
            FailureClass::RateLimited(None).name(),
            FailureNote {
                status: Some(429),
                retry_after: Some("12\n0"),
                body: Some(&body),
                secrets: &[secret],
                ..FailureNote::default()
            },
            true,
            Some(30),
        );
        let rows = app.failures().snapshot(1, None);
        assert_eq!(rows[0].status, Some(429));
        assert_eq!(rows[0].retry_after.as_deref(), Some("12 0"));
        assert!(rows[0].cooled);
        assert_eq!(rows[0].cooling_seconds, Some(30));
        assert!(!rows[0].note.contains(secret), "{}", rows[0].note);
        assert!(!rows[0].note.chars().any(char::is_control));
        assert_eq!(
            rows[0].note.chars().count(),
            crate::failures::MAX_NOTE_CHARS
        );
    }

    #[test]
    fn a_ring_snippet_with_split_key_material_is_withheld() {
        let app = App::with_header_timeout(
            Config::parse("").unwrap(),
            HashMap::new(),
            Policy::default(),
            Duration::from_secs(1),
        );
        let secret_text = ["sk-split", "-secret-0123456789"].concat();
        let secret = secret_text.as_str();
        let body = format!("{}\n{}  {}", "x".repeat(300), &secret[..6], &secret[6..]);
        record_ring(
            &app,
            "s",
            "a",
            FailureKind::RequestError.label(),
            FailureNote {
                status: Some(402),
                body: Some(&body),
                secrets: &[secret],
                ..FailureNote::default()
            },
            false,
            None,
        );
        let rows = app.failures().snapshot(1, None);
        assert_eq!(rows[0].note, WITHHELD_NOTE);
        assert_eq!(rows[0].class, "request_error");
        assert!(!rows[0].cooled);
    }

    #[test]
    fn a_phrase_matched_quota_on_402_stays_key_wide() {
        let scope = Scope {
            route: "/chat".to_owned(),
            model: "a-model".to_owned(),
        };
        assert_eq!(
            failure_scope(FailureClass::QuotaExhausted, Some(402), Some(&scope), false),
            Some(&scope),
            "a built-in 402 quota is still scoped"
        );
        assert_eq!(
            failure_scope(FailureClass::QuotaExhausted, Some(402), Some(&scope), true),
            None,
            "a configured quota phrase cools the whole key"
        );
        assert_eq!(
            failure_scope(
                FailureClass::RateLimited(None),
                Some(402),
                Some(&scope),
                false
            ),
            Some(&scope)
        );
    }

    use std::collections::VecDeque;

    const REQUEST_SIZE_402: &str = r#"{"error":{"message":"This request requires more credits, or fewer max_tokens. You requested up to 256000 tokens, but can only afford 199460."}}"#;

    #[derive(Default)]
    struct MockUpstream {
        calls: Vec<String>,
        replies: HashMap<String, VecDeque<(u16, &'static str)>>,
    }

    async fn mock_upstream(State(mock): State<Arc<Mutex<MockUpstream>>>, req: Request) -> Response {
        let key = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .trim_start_matches("Bearer ")
            .to_owned();
        let (status, body) = {
            let mut mock = mock.lock().unwrap();
            mock.calls.push(key.clone());
            mock.replies
                .get_mut(&key)
                .and_then(VecDeque::pop_front)
                .unwrap_or((200, "{\"ok\":true}"))
        };
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = StatusCode::from_u16(status).unwrap();
        response
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    struct ProxyRig {
        app: Arc<App>,
        relay: String,
        mock: Arc<Mutex<MockUpstream>>,
        client: reqwest::Client,
    }

    impl ProxyRig {
        async fn new(upstream_extra: &str) -> Self {
            let mock = Arc::new(Mutex::new(MockUpstream::default()));
            let mock_url = serve(
                Router::new()
                    .fallback(mock_upstream)
                    .with_state(Arc::clone(&mock)),
            )
            .await;
            let text = format!(
                "[upstream.zen]\nbase_url = \"{mock_url}/v1\"\n{upstream_extra}\
                 [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
                 [keys.b]\nupstream = \"zen\"\nfile = \"/unused/b\"\n\
                 [streams.client]\ntoken_sha256 = \"{}\"\nchain = [\"a\", \"b\"]\n",
                sha256_hex("tok")
            );
            let app = App::with_header_timeout(
                Config::parse(&text).unwrap(),
                HashMap::from([
                    ("a".to_owned(), Secret::new("up-a")),
                    ("b".to_owned(), Secret::new("up-b")),
                ]),
                Policy::default(),
                Duration::from_secs(5),
            );
            let relay = serve(router(Arc::clone(&app))).await;
            Self {
                app,
                relay,
                mock,
                client: reqwest::Client::new(),
            }
        }

        fn script(&self, key: &str, status: u16, body: &'static str) {
            self.mock
                .lock()
                .unwrap()
                .replies
                .entry(key.to_owned())
                .or_default()
                .push_back((status, body));
        }

        fn keys_called(&self) -> Vec<String> {
            self.mock.lock().unwrap().calls.clone()
        }

        fn post(&self) -> reqwest::RequestBuilder {
            self.client
                .post(format!("{}/zen/chat/completions", self.relay))
                .bearer_auth("tok")
                .body("{}")
        }
    }

    fn cooling(app: &App, key: &str) -> Option<FailureClass> {
        app.health()
            .lock()
            .unwrap()
            .cooling_class(key, Instant::now())
    }

    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    #[test]
    fn a_daily_quota_failure_records_its_reset_for_publishers() {
        // 2026-01-01T23:59:57Z: the configured daily reset is three seconds out.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_311_997);
        let text = format!(
            "[upstream.zen]\nbase_url = \"https://example.invalid/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/unused/a\"\n\
             quota_resets = \"daily 00:00 UTC\"\n\
             [streams.client]\ntoken_sha256 = \"{}\"\nchain = [\"a\"]\n",
            sha256_hex("tok")
        );
        let app = App::with_header_timeout_and_clock(
            Config::parse(&text).unwrap(),
            HashMap::from([("a".to_owned(), Secret::new("up-a"))]),
            Policy::default(),
            Duration::from_secs(5),
            Arc::new(FixedClock(now)),
        );
        let snapshot = app.snapshot();
        record(
            &app,
            &snapshot,
            "client",
            "a",
            FailureClass::QuotaExhausted,
            FailureNote::default(),
        );
        let health = app.health().lock().unwrap();
        assert_eq!(
            health.cooling_reset_at("a", Instant::now()),
            Some(now + Duration::from_secs(3))
        );
        assert_eq!(
            health.cooling_class("a", Instant::now()),
            Some(FailureClass::QuotaExhausted)
        );
    }

    #[tokio::test]
    async fn a_402_request_size_error_fails_over_without_cooling_the_key() {
        let rig = ProxyRig::new("").await;
        rig.script("up-a", 402, REQUEST_SIZE_402);

        let response = rig.post().send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "{\"ok\":true}");
        assert_eq!(rig.keys_called(), ["up-a", "up-b"]);
        assert!(
            rig.app
                .health()
                .lock()
                .unwrap()
                .is_available("a", Instant::now())
        );
        assert_eq!(cooling(&rig.app, "a"), None);

        let rows = rig.app.failures().snapshot(10, Some("a"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].class, FailureKind::RequestError.label());
        assert_eq!(rows[0].status, Some(402));
        assert!(!rows[0].cooled);
    }

    #[tokio::test]
    async fn the_last_request_size_answer_is_relayed_when_every_key_refuses_it() {
        let last = r#"{"error":{"message":"can only afford 100 tokens"}}"#;
        let rig = ProxyRig::new("").await;
        rig.script("up-a", 402, REQUEST_SIZE_402);
        rig.script("up-b", 402, last);

        let response = rig.post().send().await.unwrap();
        assert_eq!(response.status(), 402);
        assert_eq!(response.text().await.unwrap(), last);
        assert_eq!(rig.keys_called(), ["up-a", "up-b"]);
        for key in ["a", "b"] {
            assert!(
                rig.app
                    .health()
                    .lock()
                    .unwrap()
                    .is_available(key, Instant::now())
            );
            assert_eq!(cooling(&rig.app, key), None);
        }

        let rows = rig.app.failures().snapshot(10, None);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.class == FailureKind::RequestError.label())
        );
        assert!(rows.iter().all(|row| !row.cooled));
    }

    #[tokio::test]
    async fn a_configured_request_error_phrase_on_a_400_fails_over_without_cooling() {
        let rig = ProxyRig::new("request_error_phrases = [\"prompt is oversized\"]\n").await;
        rig.script("up-a", 400, "prompt is oversized");

        let response = rig.post().send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "{\"ok\":true}");
        assert_eq!(rig.keys_called(), ["up-a", "up-b"]);
        assert_eq!(cooling(&rig.app, "a"), None);

        let rows = rig.app.failures().snapshot(10, Some("a"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].class, FailureKind::RequestError.label());
        assert_eq!(rows[0].status, Some(400));
        assert!(!rows[0].cooled);
    }

    #[tokio::test]
    async fn a_402_without_a_matching_phrase_cools_the_key_and_moves_on() {
        let rig = ProxyRig::new("request_error_phrases = [\"prompt is oversized\"]\n").await;
        rig.script(
            "up-a",
            402,
            "{\"error\":{\"message\":\"insufficient credits\"}}",
        );

        let response = rig.post().send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "{\"ok\":true}");
        assert_eq!(rig.keys_called(), ["up-a", "up-b"]);
        assert_eq!(cooling(&rig.app, "a"), Some(FailureClass::QuotaExhausted));
        assert!(
            !rig.app
                .health()
                .lock()
                .unwrap()
                .is_available("a", Instant::now())
        );

        let rows = rig.app.failures().snapshot(10, Some("a"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].class, FailureClass::QuotaExhausted.name());
        assert_eq!(rows[0].status, Some(402));
        assert!(rows[0].cooled);
    }

    #[tokio::test]
    async fn a_body_past_the_member_scan_cap_is_refused_without_being_forwarded() {
        let rig = ProxyRig::new("drop_params = [\"stream_options\"]\n").await;
        let body = format!("{{{}\"model\":\"gpt-4o\"}}", "\"a\":1,".repeat(4096));
        let response = rig.post().body(body).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = response.text().await.unwrap();
        assert!(text.contains("too_many_members"), "{text}");
        assert!(rig.keys_called().is_empty(), "no key was tried");
    }
}
