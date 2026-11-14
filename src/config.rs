use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::chain::Strategy;
use crate::proxy::PASSTHROUGH_STREAM;
use crate::quota::Reset;

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    UnknownUpstream {
        key: String,
        upstream: String,
    },
    UnknownKey {
        stream: String,
        key: String,
    },
    EmptyChain(String),
    BadTokenHash(String),
    DuplicateToken(String, String),
    InsecureUpstream(String),
    InvalidRateLimit(String),
    BadQuotaResets {
        key: String,
        reason: String,
    },
    EmptyModelAlias(String),
    EmptyDropParam(String),
    BadFallback {
        upstream: String,
        reason: String,
    },
    BadForward {
        upstream: String,
        reason: String,
    },
    BadUpstream {
        upstream: String,
        reason: String,
    },
    PassthroughKey {
        key: String,
        upstream: String,
    },
    FallbackWithoutKeys {
        stream: String,
        upstream: String,
        fallback: String,
    },
    BadAdminTokenHash,
    AdminTokenReused(String),
    ReservedStreamName(String),
    AdminListenConflict,
    BadTunnel(String),
    BadLog(String),
    BadReload(String),
    BadAuth(String),
    BadPeers(String),
    UnknownPlugin {
        name: String,
        known: Vec<String>,
    },
    PluginConfig {
        name: String,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "invalid TOML: {e}"),
            Self::UnknownUpstream { key, upstream } => {
                write!(f, "key `{key}` refers to unknown upstream `{upstream}`")
            }
            Self::UnknownKey { stream, key } => {
                write!(f, "stream `{stream}` refers to unknown key `{key}`")
            }
            Self::EmptyChain(stream) => write!(f, "stream `{stream}` has an empty chain"),
            Self::BadTokenHash(stream) => {
                write!(
                    f,
                    "stream `{stream}` has a token_sha256 that is not 64 hex characters"
                )
            }
            Self::DuplicateToken(a, b) => {
                write!(f, "two streams share the same token hash: `{a}` and `{b}`")
            }
            Self::InsecureUpstream(name) => {
                write!(f, "upstream `{name}` must use an https base_url")
            }
            Self::InvalidRateLimit(stream) => {
                write!(
                    f,
                    "stream `{stream}` has a rate_limit_per_minute of 0; it must be at least 1"
                )
            }
            Self::BadQuotaResets { key, reason } => {
                write!(f, "key `{key}` has an invalid quota_resets: {reason}")
            }
            Self::EmptyModelAlias(name) => {
                write!(
                    f,
                    "upstream `{name}` has an empty model_aliases key or value"
                )
            }
            Self::EmptyDropParam(name) => {
                write!(f, "upstream `{name}` has an empty drop_params entry")
            }
            Self::BadFallback { upstream, reason } => {
                write!(f, "upstream `{upstream}` has an invalid fallback: {reason}")
            }
            Self::BadForward { upstream, reason } => {
                write!(f, "upstream `{upstream}` has an invalid forward: {reason}")
            }
            Self::BadUpstream { upstream, reason } => {
                write!(f, "upstream `{upstream}` has an invalid setting: {reason}")
            }
            Self::FallbackWithoutKeys {
                stream,
                upstream,
                fallback,
            } => {
                write!(
                    f,
                    "stream `{stream}` uses upstream `{upstream}`, which falls back to `{fallback}`, but the stream's chain has no `{fallback}` key"
                )
            }
            Self::PassthroughKey { key, upstream } => {
                write!(
                    f,
                    "key `{key}` belongs to upstream `{upstream}`, which is a passthrough and never uses Reiki's keys"
                )
            }
            Self::BadAdminTokenHash => {
                write!(
                    f,
                    "admin token_sha256 is not 64 hex characters (it must not be empty)"
                )
            }
            Self::AdminTokenReused(stream) => write!(
                f,
                "admin token_sha256 is the same as the token of stream `{stream}`, which would give that stream admin access"
            ),
            Self::ReservedStreamName(name) => {
                write!(f, "stream name `{name}` is reserved for unauthenticated passthrough traffic")
            }
            Self::BadTunnel(reason) => write!(f, "invalid [tunnel]: {reason}"),
            Self::BadLog(reason) => write!(f, "invalid [log]: {reason}"),
            Self::BadReload(reason) => write!(f, "invalid [reload]: {reason}"),
            Self::BadAuth(reason) => write!(f, "invalid [auth]: {reason}"),
            Self::BadPeers(reason) => write!(f, "invalid [peers]: {reason}"),
            Self::AdminListenConflict => {
                write!(f, "admin listen must differ from the client listen address")
            }
            Self::UnknownPlugin { name, known } => {
                if known.is_empty() {
                    write!(
                        f,
                        "plugin `{name}` is not compiled in; no plugins are compiled in"
                    )
                } else {
                    write!(
                        f,
                        "plugin `{name}` is not compiled in; compiled-in plugins: {}",
                        known.join(", ")
                    )
                }
            }
            Self::PluginConfig { name, message } => {
                write!(f, "plugin `{name}`: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Plugin names that were renamed, as (old, current).
const RENAMED_PLUGINS: &[(&str, &str)] = &[("gov", "controlled")];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub upstream: BTreeMap<String, Upstream>,
    #[serde(default)]
    pub keys: BTreeMap<String, KeyConfig>,
    #[serde(default)]
    pub streams: BTreeMap<String, StreamConfig>,
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    /// Optional CONNECT-only forward proxy that copies bytes without reading them.
    #[serde(default)]
    pub tunnel: Option<TunnelConfig>,
    /// Where Reiki's own log lines go and how much it says.
    #[serde(default)]
    pub log: LogConfig,
    /// Reloading the config when its files change.
    #[serde(default)]
    pub reload: ReloadConfig,
    /// Failed token checks on the client and admin listeners.
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(skip)]
    quota_resets: BTreeMap<String, Reset>,
    #[serde(default)]
    pub plugins: BTreeMap<String, toml::Table>,
    /// Health sharing between Reiki instances; absent means off.
    #[serde(default)]
    pub peers: Option<PeersConfig>,
}

/// `[peers]`: scalar keys are settings, each `[peers.<name>]` sub-table is a peer to read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "toml::Table")]
pub struct PeersConfig {
    /// Master switch, so the table can exist and do nothing.
    pub enabled: bool,
    /// Serve `GET /v1/health` on `listen` to holders of the peer token.
    pub publish: bool,
    /// Address of the peer listener, never the client, admin or tunnel one.
    pub listen: Option<String>,
    /// SHA-256 of the read-only peer token: shorthand for one `[[peers.tokens]]` named `default`.
    pub peer_token_sha256: Option<String>,
    /// Every peer token, the shorthand first; publishing needs at least one.
    pub tokens: Vec<PeerToken>,
    /// Name published as `instance`; `reiki` when unset.
    pub instance: Option<String>,
    /// Reader poll period; data older than three of them counts as unknown.
    pub poll_interval_secs: u64,
    /// Publish per-key labels and states, not only counts per upstream.
    pub expose_key_labels: bool,
    /// Publish load, memory, disk use and active sessions.
    pub expose_host_stats: bool,
    /// Client header whose distinct values count as active sessions.
    pub session_header: String,
    /// How long a session id counts as active after its last request.
    pub session_window_secs: u64,
    /// Period of `host` events on `/v1/health/stream`.
    pub stream_interval_secs: u64,
    /// Open `/v1/health/stream` connections allowed at once.
    pub max_subscribers: u32,
    /// Open stream connections allowed from one client address.
    pub max_subscribers_per_peer: u32,
    /// Open stream connections allowed for one peer token.
    pub max_subscribers_per_token: u32,
    /// Published cooldowns are rounded up to a multiple of this; 0 publishes them exactly.
    pub cooldown_bucket_secs: u64,
    /// PEM certificate chain the peer listener serves HTTPS with; set with `tls_key`.
    pub tls_cert: Option<String>,
    /// PEM private key for `tls_cert`, private to its owner.
    pub tls_key: Option<String>,
    /// How a `tls_key` that group or others can access is treated.
    pub tls_key_permissions: CaKeyPermissions,
    pub peers: BTreeMap<String, PeerEntry>,
}

/// One `[[peers.tokens]]`: a named read-only peer token. The name may be logged; the token never.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerToken {
    pub name: String,
    pub token_sha256: String,
}

/// The name the `peer_token_sha256` shorthand gets.
pub const DEFAULT_PEER_TOKEN: &str = "default";

/// Kept apart from the peer sub-tables so a misspelt setting is refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeersSettings {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    publish: bool,
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    peer_token_sha256: Option<String>,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default = "default_poll_interval_secs")]
    poll_interval_secs: u64,
    #[serde(default)]
    expose_key_labels: bool,
    #[serde(default)]
    expose_host_stats: bool,
    #[serde(default = "default_session_header")]
    session_header: String,
    #[serde(default = "default_session_window_secs")]
    session_window_secs: u64,
    #[serde(default = "default_stream_interval_secs")]
    stream_interval_secs: u64,
    #[serde(default = "default_max_subscribers")]
    max_subscribers: u32,
    #[serde(default = "default_max_subscribers_per_peer")]
    max_subscribers_per_peer: u32,
    #[serde(default = "default_max_subscribers_per_token")]
    max_subscribers_per_token: u32,
    #[serde(default)]
    tokens: Vec<PeerToken>,
    #[serde(default = "default_cooldown_bucket_secs")]
    cooldown_bucket_secs: u64,
    #[serde(default)]
    tls_cert: Option<String>,
    #[serde(default)]
    tls_key: Option<String>,
    #[serde(default)]
    tls_key_permissions: CaKeyPermissions,
}

fn default_cooldown_bucket_secs() -> u64 {
    300
}

fn default_max_subscribers_per_token() -> u32 {
    8
}

fn default_stream_interval_secs() -> u64 {
    5
}

fn default_max_subscribers() -> u32 {
    16
}

fn default_max_subscribers_per_peer() -> u32 {
    4
}

fn default_poll_interval_secs() -> u64 {
    30
}

fn default_session_header() -> String {
    "x-opencode-session".to_owned()
}

fn default_session_window_secs() -> u64 {
    300
}

/// One `[peers.<name>]`: another instance whose health `reiki peers` reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEntry {
    /// Base URL of the peer listener: https, or http on loopback only.
    pub url: String,
    /// File holding the token that peer issued to this instance.
    pub token_file: String,
    /// PEM CA certificates trusted for this peer and nothing else; required for https.
    #[serde(default)]
    pub ca_file: Option<String>,
}

impl TryFrom<toml::Table> for PeersConfig {
    type Error = String;

    fn try_from(mut table: toml::Table) -> Result<Self, Self::Error> {
        let names: Vec<String> = table
            .iter()
            .filter(|(_, value)| value.is_table())
            .map(|(name, _)| name.clone())
            .collect();
        let mut peers = BTreeMap::new();
        for name in names {
            let Some(value) = table.remove(&name) else {
                continue;
            };
            let entry: PeerEntry = value
                .try_into()
                .map_err(|e| format!("[peers.{name}]: {e}"))?;
            peers.insert(name, entry);
        }
        let settings: PeersSettings = toml::Value::Table(table)
            .try_into()
            .map_err(|e| format!("[peers]: {e}"))?;
        let tokens = settings
            .peer_token_sha256
            .iter()
            .map(|hash| PeerToken {
                name: DEFAULT_PEER_TOKEN.to_owned(),
                token_sha256: hash.clone(),
            })
            .chain(settings.tokens)
            .collect();
        Ok(Self {
            enabled: settings.enabled,
            publish: settings.publish,
            listen: settings.listen,
            peer_token_sha256: settings.peer_token_sha256,
            tokens,
            instance: settings.instance,
            poll_interval_secs: settings.poll_interval_secs,
            expose_key_labels: settings.expose_key_labels,
            expose_host_stats: settings.expose_host_stats,
            session_header: settings.session_header,
            session_window_secs: settings.session_window_secs,
            stream_interval_secs: settings.stream_interval_secs,
            max_subscribers: settings.max_subscribers,
            max_subscribers_per_peer: settings.max_subscribers_per_peer,
            max_subscribers_per_token: settings.max_subscribers_per_token,
            cooldown_bucket_secs: settings.cooldown_bucket_secs,
            tls_cert: settings.tls_cert,
            tls_key: settings.tls_key,
            tls_key_permissions: settings.tls_key_permissions,
            peers,
        })
    }
}

impl PeersConfig {
    /// True when the peer listener should run and answer.
    pub fn publishing(&self) -> bool {
        self.enabled && self.publish
    }

    /// The certificate and key files when the peer listener serves HTTPS.
    pub fn tls_files(&self) -> Option<(&str, &str)> {
        Some((self.tls_cert.as_deref()?, self.tls_key.as_deref()?))
    }

    /// The `peers:` line `reiki check` prints.
    pub fn check_line(peers: Option<&Self>) -> String {
        let Some(peers) = peers.filter(|p| p.enabled) else {
            return "peers: disabled".to_owned();
        };
        let tls = if peers.tls_files().is_some() {
            " over https"
        } else {
            ""
        };
        let publish = match (&peers.listen, peers.publish) {
            (Some(listen), true) => format!(
                "publishing on {listen}{tls} (key labels {})",
                if peers.expose_key_labels {
                    "shown"
                } else {
                    "hidden"
                }
            ),
            _ => "not publishing".to_owned(),
        };
        let publish = if peers.publish && peers.tokens.len() > 1 {
            let names: Vec<&str> = peers.tokens.iter().map(|t| t.name.as_str()).collect();
            format!("{publish}, {} token(s)[{}]", names.len(), names.join(", "))
        } else {
            publish
        };
        let names = peers.peers.keys().cloned().collect::<Vec<_>>();
        let host = if peers.expose_host_stats {
            format!(
                ", host stats and sessions from `{}` over {}s",
                peers.session_header, peers.session_window_secs
            )
        } else {
            String::new()
        };
        format!(
            "peers: enabled, {publish}, {} peer(s)[{}], poll every {}s{host}",
            names.len(),
            names.join(", "),
            peers.poll_interval_secs
        )
    }
}

/// Reiki's service log, not the request log plugin.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// A level (`info`) or a filter such as `info,reiki::tunnel=debug`. `RUST_LOG`, when set, wins at startup.
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Also write lines to this file, which must be writable by the service user.
    #[serde(default)]
    pub file: Option<String>,
    /// Layout of the file: `text` or `json`.
    #[serde(default)]
    pub format: LogFormat,
    /// Keep writing to stderr, which a service manager collects.
    #[serde(default = "default_true")]
    pub stderr: bool,
    /// Rotate the file when the next line would pass this size; 0 never rotates.
    #[serde(default = "default_log_file_max_bytes")]
    pub file_max_bytes: u64,
    /// Rotated files kept as `<file>.1` (newest) to `<file>.N`.
    #[serde(default = "default_log_file_keep")]
    pub file_keep: u32,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
            format: LogFormat::Text,
            stderr: true,
            file_max_bytes: default_log_file_max_bytes(),
            file_keep: default_log_file_keep(),
        }
    }
}

/// `[reload]`: watch the config and key files and reload when they change.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReloadConfig {
    /// Reload when the config file, a key file or a file a plugin reads has changed. Off by default.
    #[serde(default)]
    pub watch: bool,
    /// Seconds between checks. A change is applied once the files have stayed unchanged for one more check.
    #[serde(default = "default_reload_interval")]
    pub interval_secs: u64,
}

impl Default for ReloadConfig {
    fn default() -> Self {
        Self {
            watch: false,
            interval_secs: default_reload_interval(),
        }
    }
}

fn default_reload_interval() -> u64 {
    5
}

/// `[auth]`: how failed token checks on the client and admin listeners are slowed down.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Count failures per peer address and refuse that peer while the window lasts.
    #[serde(default = "default_true")]
    pub backoff: bool,
    /// Failures one address may make inside `window_secs` before it is refused.
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    /// How long a failure counts, and how long a refused address must wait.
    #[serde(default = "default_failure_window_secs")]
    pub window_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            backoff: true,
            max_failures: default_max_failures(),
            window_secs: default_failure_window_secs(),
        }
    }
}

fn default_max_failures() -> u32 {
    10
}

fn default_failure_window_secs() -> u64 {
    60
}

fn default_log_level() -> String {
    "info".to_owned()
}

fn default_true() -> bool {
    true
}

fn default_log_file_max_bytes() -> u64 {
    50 * 1024 * 1024
}

fn default_log_file_keep() -> u32 {
    5
}

fn default_listen() -> String {
    "127.0.0.1:4097".to_owned()
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    pub base_url: String,
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    #[serde(default = "default_auth_format")]
    pub auth_format: String,
    #[serde(default)]
    pub model_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub drop_params: Vec<String>,
    /// Negotiate HTTP/2 with this upstream when it offers it. Off keeps HTTP/1.1.
    #[serde(default)]
    pub http2: bool,
    /// Send the client's own `Accept-Encoding` upstream instead of `identity`.
    /// Reiki decodes compressed responses itself so errors can still be read.
    #[serde(default)]
    pub forward_accept_encoding: bool,
    /// Most bytes a response body may hold after decoding. An over-limit body becomes a 502
    /// before a buffered response is sent, so a small compressed body cannot expand without bound.
    #[serde(default = "default_max_decoded_bytes")]
    pub max_decoded_bytes: u64,
    /// Longest gap between response body chunks before the body is abandoned. Defaults to 300.
    #[serde(default = "default_response_idle_timeout_secs")]
    pub response_idle_timeout_secs: u64,
    /// Longest the whole response body may take after its head arrives. Unset means no total
    /// limit; the idle timeout still applies.
    #[serde(default)]
    pub response_timeout_secs: Option<u64>,
    /// Send header names in Title-Case (`Content-Type`) on HTTP/1.1. Ignored with `http2`.
    #[serde(default)]
    pub title_case_headers: bool,
    /// Send `Connection: keep-alive` explicitly; HTTP/1.1 connections persist without it.
    #[serde(default)]
    pub send_keep_alive: bool,
    /// Relay every upstream error status to the client as it is. No key is cooled, disabled or
    /// replayed, so an authorisation error from a non-LLM upstream cannot take the key out of rotation.
    #[serde(default)]
    pub pass_errors: bool,
    /// Re-send a quota-exhausted 429 once on the same key, after a short pause, before cooling
    /// the key. For upstreams that sometimes answer a good key with a spurious quota 429.
    #[serde(default)]
    pub confirm_quota: bool,
    /// Hold back the disable of the first key a request finds unauthorized until a later key
    /// serves that same request, so one bad request cannot take a good key out of rotation.
    #[serde(default)]
    pub hold_unauthorized: bool,
    /// Where a request goes when every key of this upstream is unavailable.
    #[serde(default)]
    pub fallback: Option<Fallback>,
    /// Act as a plain reverse proxy: the client's own credentials go upstream untouched,
    /// no Reiki token is needed and no key from the chain is used.
    #[serde(default)]
    pub passthrough: bool,
    /// Run this command for each request instead of Reiki's HTTP clients.
    /// The API token and outbound headers are written to the process stdin, never argv.
    #[serde(default)]
    pub forward: Option<Forward>,
}

/// A per-request child process that performs the upstream call.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Forward {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub pass_env: Vec<String>,
    pub socket: Option<std::path::PathBuf>,
    pub expect_uid: Option<u32>,
    /// Most copies of `command` running at once; further requests get a 502. Defaults to 16.
    pub max_concurrent: Option<u32>,
    /// Relay the forwarder's body as it arrives instead of buffering it whole (8 MiB cap).
    /// The request timeout then bounds only the wait for the response head.
    pub streaming: bool,
}

/// One hop to another upstream, used only when the chain has no usable key left here.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    pub upstream: String,
    /// Applied to the request before the target upstream's own `model_aliases`; `"*"` matches any model.
    #[serde(default)]
    pub model_aliases: BTreeMap<String, String>,
}

fn default_max_decoded_bytes() -> u64 {
    64 * 1024 * 1024
}

fn default_response_idle_timeout_secs() -> u64 {
    300
}

fn default_auth_header() -> String {
    "authorization".to_owned()
}

fn default_auth_format() -> String {
    "Bearer {key}".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    pub upstream: String,
    pub file: String,
    #[serde(default)]
    pub quota_resets: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    pub token_sha256: String,
    pub chain: Vec<String>,
    #[serde(default = "default_strategy")]
    pub strategy: StrategyConfig,
    #[serde(default)]
    pub rate_limit_per_minute: Option<u32>,
}

fn default_strategy() -> StrategyConfig {
    StrategyConfig::Sticky
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    pub listen: String,
    pub token_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelConfig {
    pub listen: String,
    /// Destinations a client may open: `host`, `host:port` (default 443), `*.host` or `*`.
    pub allow: Vec<String>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Lets the tunnel reach loopback addresses. For tests and local development only.
    #[serde(default)]
    pub allow_loopback: bool,
    /// Refuses private-range addresses (RFC 1918, shared 100.64.0.0/10, IPv6 unique-local),
    /// so a broad allowlist reaches the internet and not the local network.
    #[serde(default)]
    pub deny_private: bool,
    /// Terminates TLS for listed hosts so plugins and key handling apply to tunnelled traffic.
    #[serde(default)]
    pub intercept: Option<InterceptConfig>,
}

fn default_connect_timeout() -> u64 {
    10
}

/// TLS interception for the tunnel: Reiki answers as the listed hosts using certificates
/// signed by a CA the client trusts, then relays each request like any other.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterceptConfig {
    /// PEM certificate of the signing CA.
    pub ca_cert: String,
    /// PEM private key of the signing CA.
    pub ca_key: String,
    /// How long a minted certificate is valid.
    #[serde(default = "default_leaf_ttl_hours")]
    pub leaf_ttl_hours: u64,
    /// Whether a CONNECT to a host served from a stream must carry that stream's token.
    /// Turn it off only when the tunnel listener is reachable by trusted clients alone.
    #[serde(default = "default_true")]
    pub require_auth: bool,
    /// What to do when the CA key file can be read by its group or by everyone.
    #[serde(default)]
    pub ca_key_permissions: CaKeyPermissions,
    pub hosts: Vec<InterceptHost>,
}

/// How a CA key file that is not private to its owner is treated.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaKeyPermissions {
    /// Refuse to load the CA.
    #[default]
    Error,
    /// Load it and log a warning.
    Warn,
}

fn default_leaf_ttl_hours() -> u64 {
    24
}

/// One intercepted host and the upstream its requests are relayed to.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InterceptHost {
    pub host: String,
    pub upstream: String,
    /// The stream whose key chain serves these requests. Omit it for a `passthrough` upstream.
    #[serde(default)]
    pub stream: Option<String>,
    /// Path prefixes relayed as the client sent them, to the upstream's host. Empty keeps only
    /// the upstream's base path, rewritten onto its base URL.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StrategyConfig {
    Sticky,
    RoundRobin,
}

impl From<StrategyConfig> for Strategy {
    fn from(value: StrategyConfig) -> Self {
        match value {
            StrategyConfig::Sticky => Strategy::Sticky,
            StrategyConfig::RoundRobin => Strategy::RoundRobin,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut config: Config =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.rename_old_plugins()?;
        config.resolve_quota_resets()?;
        config.validate()?;
        Ok(config)
    }

    /// Plugin tables under a name the plugin used to have move to the current one.
    fn rename_old_plugins(&mut self) -> Result<(), ConfigError> {
        for (old, new) in RENAMED_PLUGINS {
            let Some(table) = self.plugins.remove(*old) else {
                continue;
            };
            if self.plugins.contains_key(*new) {
                return Err(ConfigError::PluginConfig {
                    name: (*old).to_owned(),
                    message: format!(
                        "`{old}` is the old name of `{new}`; keep only `[plugins.{new}]`"
                    ),
                });
            }
            tracing::warn!(old, new, "plugin table uses an old name; rename it");
            self.plugins.insert((*new).to_owned(), table);
        }
        Ok(())
    }

    /// The parsed reset schedule for a key, if it has one.
    pub fn quota_reset(&self, key: &str) -> Option<Reset> {
        self.quota_resets.get(key).copied()
    }

    fn resolve_quota_resets(&mut self) -> Result<(), ConfigError> {
        let mut resets = BTreeMap::new();
        for (name, key) in &self.keys {
            if let Some(value) = &key.quota_resets {
                let reset = Reset::parse(value).map_err(|reason| ConfigError::BadQuotaResets {
                    key: name.clone(),
                    reason,
                })?;
                resets.insert(name.clone(), reset);
            }
        }
        self.quota_resets = resets;
        if let Some(tunnel) = &self.tunnel {
            let reason = if tunnel.allow.is_empty() {
                Some("allow lists no destinations".to_owned())
            } else if let Some(bad) = tunnel.allow.iter().find(|r| !crate::tunnel::valid_rule(r)) {
                Some(format!("`{bad}` is not a valid allow entry"))
            } else if tunnel.connect_timeout_secs == 0 {
                Some("connect_timeout_secs must be at least 1".to_owned())
            } else if tunnel.listen == self.listen
                || self
                    .admin
                    .as_ref()
                    .is_some_and(|a| a.listen == tunnel.listen)
            {
                Some("listen must differ from the client and admin listen addresses".to_owned())
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(ConfigError::BadTunnel(reason));
            }
            if let Some(intercept) = &tunnel.intercept {
                if let Some(reason) = self.intercept_problem(tunnel, intercept) {
                    return Err(ConfigError::BadTunnel(format!("intercept: {reason}")));
                }
            }
        }
        Ok(())
    }

    fn log_problem(&self) -> Option<String> {
        let log = &self.log;
        if tracing_subscriber::EnvFilter::try_new(&log.level).is_err() {
            return Some(format!("`{}` is not a valid level or filter", log.level));
        }
        if log.file.as_deref().is_some_and(|f| !f.starts_with('/')) {
            return Some("file must be an absolute path".to_owned());
        }
        if log.file.is_none() && !log.stderr {
            return Some("stderr is off and no file is set, so nothing would be logged".to_owned());
        }
        if log.file_keep > 100 {
            return Some("file_keep must be at most 100".to_owned());
        }
        None
    }

    /// What is wrong with an `[tunnel.intercept]` table, if anything.
    fn intercept_problem(
        &self,
        tunnel: &TunnelConfig,
        intercept: &InterceptConfig,
    ) -> Option<String> {
        if intercept.hosts.is_empty() {
            return Some("hosts lists no hosts".to_owned());
        }
        if intercept.leaf_ttl_hours == 0 || intercept.leaf_ttl_hours > 720 {
            return Some("leaf_ttl_hours must be between 1 and 720".to_owned());
        }
        let mut seen = std::collections::BTreeSet::new();
        for entry in &intercept.hosts {
            let host = &entry.host;
            if !crate::intercept::valid_host(host) {
                return Some(format!("`{host}` must be a plain lowercase DNS name"));
            }
            if !seen.insert(host.as_str()) {
                return Some(format!("`{host}` is listed twice"));
            }
            if !crate::tunnel::allowed(&tunnel.allow, host, 443) {
                return Some(format!("`{host}` is not covered by the tunnel allow list"));
            }
            let Some(upstream) = self.upstream.get(&entry.upstream) else {
                return Some(format!(
                    "`{host}` names an unknown upstream `{}`",
                    entry.upstream
                ));
            };
            if upstream.base_url.starts_with("https://")
                && crate::intercept::base_host(&upstream.base_url).as_deref() != Some(host.as_str())
            {
                return Some(format!(
                    "upstream `{}` does not live at `{host}`",
                    entry.upstream
                ));
            }
            if let Some(bad) = entry
                .paths
                .iter()
                .find(|p| !p.starts_with('/') || p.contains('?'))
            {
                return Some(format!("`{bad}` must be a path prefix starting with `/`"));
            }
            match (&entry.stream, upstream.passthrough) {
                (Some(_), true) => {
                    return Some(format!(
                        "`{host}` uses a passthrough upstream, so it takes no stream"
                    ));
                }
                (None, false) => return Some(format!("`{host}` needs a stream")),
                (Some(name), false) => {
                    let Some(stream) = self.streams.get(name) else {
                        return Some(format!("`{host}` names an unknown stream `{name}`"));
                    };
                    let serves = stream.chain.iter().any(|key| {
                        self.keys
                            .get(key)
                            .is_some_and(|k| k.upstream == entry.upstream)
                    });
                    if !serves {
                        return Some(format!(
                            "stream `{name}` has no key for upstream `{}`",
                            entry.upstream
                        ));
                    }
                }
                (None, true) => {}
            }
        }
        None
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(reason) = self.log_problem() {
            return Err(ConfigError::BadLog(reason));
        }
        for (name, upstream) in &self.upstream {
            // Loopback http is allowed so tests and local mocks work.
            if !upstream.base_url.starts_with("https://") && !is_loopback_http(&upstream.base_url) {
                return Err(ConfigError::InsecureUpstream(name.clone()));
            }
            if upstream.model_aliases.keys().any(String::is_empty)
                || upstream.model_aliases.values().any(String::is_empty)
            {
                return Err(ConfigError::EmptyModelAlias(name.clone()));
            }
            if upstream.drop_params.iter().any(String::is_empty) {
                return Err(ConfigError::EmptyDropParam(name.clone()));
            }
            if upstream.max_decoded_bytes == 0 {
                return Err(ConfigError::BadUpstream {
                    upstream: name.clone(),
                    reason: "max_decoded_bytes must be at least 1".to_owned(),
                });
            }
            if upstream.response_idle_timeout_secs == 0 {
                return Err(ConfigError::BadUpstream {
                    upstream: name.clone(),
                    reason: "response_idle_timeout_secs must be at least 1".to_owned(),
                });
            }
            if upstream.response_timeout_secs == Some(0) {
                return Err(ConfigError::BadUpstream {
                    upstream: name.clone(),
                    reason: "response_timeout_secs must be at least 1".to_owned(),
                });
            }
            if let Some(forward) = &upstream.forward {
                let has_command = forward.command.is_some();
                let has_socket = forward.socket.is_some();
                if has_command == has_socket {
                    return Err(ConfigError::BadForward {
                        upstream: name.clone(),
                        reason: "set exactly one of `command` or `socket`".to_owned(),
                    });
                }
                if let Some(command) = &forward.command {
                    if command.trim().is_empty() {
                        return Err(ConfigError::BadForward {
                            upstream: name.clone(),
                            reason: "command must not be empty".to_owned(),
                        });
                    }
                }
                if !has_command && (!forward.args.is_empty() || !forward.pass_env.is_empty()) {
                    return Err(ConfigError::BadForward {
                        upstream: name.clone(),
                        reason: "`args` and `pass_env` apply only to `command`".to_owned(),
                    });
                }
                if has_command && forward.expect_uid.is_some() {
                    return Err(ConfigError::BadForward {
                        upstream: name.clone(),
                        reason: "`expect_uid` applies only to `socket`".to_owned(),
                    });
                }
                if forward.max_concurrent == Some(0) {
                    return Err(ConfigError::BadForward {
                        upstream: name.clone(),
                        reason: "`max_concurrent` must be at least 1".to_owned(),
                    });
                }
                if !has_command && forward.max_concurrent.is_some() {
                    return Err(ConfigError::BadForward {
                        upstream: name.clone(),
                        reason: "`max_concurrent` applies only to `command`".to_owned(),
                    });
                }
                if let Some(socket) = &forward.socket {
                    if !socket.is_absolute() {
                        return Err(ConfigError::BadForward {
                            upstream: name.clone(),
                            reason: "socket must be an absolute path".to_owned(),
                        });
                    }
                    let len = socket.as_os_str().len();
                    if len > 103 {
                        return Err(ConfigError::BadForward {
                            upstream: name.clone(),
                            reason: "socket path must be at most 103 bytes".to_owned(),
                        });
                    }
                    if cfg!(not(unix)) {
                        return Err(ConfigError::BadForward {
                            upstream: name.clone(),
                            reason: "socket forwarding needs a Unix platform".to_owned(),
                        });
                    }
                }
                if upstream.http2 {
                    return Err(ConfigError::BadForward {
                        upstream: name.clone(),
                        reason: "http2 does not apply to a forward upstream".to_owned(),
                    });
                }
                if upstream.title_case_headers {
                    return Err(ConfigError::BadForward {
                        upstream: name.clone(),
                        reason: "title_case_headers does not apply to a forward upstream"
                            .to_owned(),
                    });
                }
            }
            if upstream.passthrough && upstream.fallback.is_some() {
                return Err(ConfigError::BadFallback {
                    upstream: name.clone(),
                    reason: "a passthrough has no keys to run out of".to_owned(),
                });
            }
            if let Some(fallback) = &upstream.fallback {
                let reason = if fallback.upstream == *name {
                    Some("it names the upstream itself")
                } else if !self.upstream.contains_key(&fallback.upstream) {
                    Some("it names an unknown upstream")
                } else if fallback
                    .model_aliases
                    .iter()
                    .any(|(k, v)| k.is_empty() || v.is_empty())
                {
                    Some("model_aliases has an empty key or value")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Err(ConfigError::BadFallback {
                        upstream: name.clone(),
                        reason: reason.to_owned(),
                    });
                }
            }
        }
        for (name, key) in &self.keys {
            let Some(upstream) = self.upstream.get(&key.upstream) else {
                return Err(ConfigError::UnknownUpstream {
                    key: name.clone(),
                    upstream: key.upstream.clone(),
                });
            };
            if upstream.passthrough {
                return Err(ConfigError::PassthroughKey {
                    key: name.clone(),
                    upstream: key.upstream.clone(),
                });
            }
        }
        let mut seen: Vec<(&str, &str)> = Vec::new();
        if self.streams.contains_key("passthrough") {
            return Err(ConfigError::ReservedStreamName("passthrough".to_owned()));
        }
        for (name, stream) in &self.streams {
            if stream.chain.is_empty() {
                return Err(ConfigError::EmptyChain(name.clone()));
            }
            for key in &stream.chain {
                if !self.keys.contains_key(key) {
                    return Err(ConfigError::UnknownKey {
                        stream: name.clone(),
                        key: key.clone(),
                    });
                }
            }
            for key in &stream.chain {
                let used = &self.keys[key].upstream;
                let Some(fallback) = &self.upstream[used].fallback else {
                    continue;
                };
                let has_target = stream
                    .chain
                    .iter()
                    .any(|k| self.keys[k].upstream == fallback.upstream);
                if !has_target {
                    return Err(ConfigError::FallbackWithoutKeys {
                        stream: name.clone(),
                        upstream: used.clone(),
                        fallback: fallback.upstream.clone(),
                    });
                }
            }
            if stream.rate_limit_per_minute == Some(0) {
                return Err(ConfigError::InvalidRateLimit(name.clone()));
            }
            let hash = stream.token_sha256.as_str();
            if !is_sha256_hex(hash) {
                return Err(ConfigError::BadTokenHash(name.clone()));
            }
            if let Some((other, _)) = seen.iter().find(|(_, h)| h.eq_ignore_ascii_case(hash)) {
                return Err(ConfigError::DuplicateToken(
                    (*other).to_owned(),
                    name.clone(),
                ));
            }
            seen.push((name.as_str(), hash));
        }
        for (name, table) in &self.plugins {
            let Some(streams) = crate::plugins::streams_of(name, table)? else {
                continue;
            };
            if let Some(stream) = streams.iter().find(|s| {
                s.as_str() != PASSTHROUGH_STREAM && !self.streams.contains_key(s.as_str())
            }) {
                return Err(ConfigError::PluginConfig {
                    name: name.clone(),
                    message: format!("stream `{stream}` is not configured"),
                });
            }
        }
        if let Some(admin) = &self.admin {
            if !is_sha256_hex(&admin.token_sha256) {
                return Err(ConfigError::BadAdminTokenHash);
            }
            let admin_hash = admin.token_sha256.to_ascii_lowercase();
            if let Some((name, _)) = self
                .streams
                .iter()
                .find(|(_, stream)| stream.token_sha256.to_ascii_lowercase() == admin_hash)
            {
                return Err(ConfigError::AdminTokenReused(name.clone()));
            }
            if admin.listen == self.listen {
                return Err(ConfigError::AdminListenConflict);
            }
        }
        if !(1..=3600).contains(&self.reload.interval_secs) {
            return Err(ConfigError::BadReload(format!(
                "interval_secs must be between 1 and 3600, got {}",
                self.reload.interval_secs
            )));
        }
        if !(1..=10_000).contains(&self.auth.max_failures) {
            return Err(ConfigError::BadAuth(format!(
                "max_failures must be between 1 and 10000, got {}",
                self.auth.max_failures
            )));
        }
        if !(1..=86_400).contains(&self.auth.window_secs) {
            return Err(ConfigError::BadAuth(format!(
                "window_secs must be between 1 and 86400, got {}",
                self.auth.window_secs
            )));
        }
        if let Some(peers) = &self.peers {
            if let Some(reason) = self.peers_problem(peers) {
                return Err(ConfigError::BadPeers(reason));
            }
        }
        Ok(())
    }

    /// What is wrong with a `[peers]` table, if anything.
    fn peers_problem(&self, peers: &PeersConfig) -> Option<String> {
        if cfg!(not(feature = "peers")) {
            return Some(
                "this binary was built without the `peers` feature; remove [peers] or rebuild with it"
                    .to_owned(),
            );
        }
        if !(1..=86_400).contains(&peers.poll_interval_secs) {
            return Some(format!(
                "poll_interval_secs must be between 1 and 86400, got {}",
                peers.poll_interval_secs
            ));
        }
        if !(10..=86_400).contains(&peers.session_window_secs) {
            return Some(format!(
                "session_window_secs must be between 10 and 86400, got {}",
                peers.session_window_secs
            ));
        }
        if !(1..=3600).contains(&peers.stream_interval_secs) {
            return Some(format!(
                "stream_interval_secs must be between 1 and 3600, got {}",
                peers.stream_interval_secs
            ));
        }
        if !(1..=256).contains(&peers.max_subscribers) {
            return Some(format!(
                "max_subscribers must be between 1 and 256, got {}",
                peers.max_subscribers
            ));
        }
        if !(1..=peers.max_subscribers).contains(&peers.max_subscribers_per_peer) {
            return Some(format!(
                "max_subscribers_per_peer must be between 1 and max_subscribers ({}), got {}",
                peers.max_subscribers, peers.max_subscribers_per_peer
            ));
        }
        if peers.cooldown_bucket_secs > 86_400 {
            return Some(format!(
                "cooldown_bucket_secs must be between 0 and 86400, got {}",
                peers.cooldown_bucket_secs
            ));
        }
        if !(1..=256).contains(&peers.max_subscribers_per_token) {
            return Some(format!(
                "max_subscribers_per_token must be between 1 and 256, got {}",
                peers.max_subscribers_per_token
            ));
        }
        let header = peers.session_header.as_str();
        if axum::http::HeaderName::from_bytes(header.as_bytes())
            .map_or(true, |name| name.as_str() != header)
        {
            return Some(format!(
                "session_header `{header}` must be a lowercase header name"
            ));
        }
        if matches!(
            header,
            "authorization" | "proxy-authorization" | "cookie" | "x-api-key" | "x-goog-api-key"
        ) {
            return Some(format!(
                "session_header `{header}` carries credentials and cannot count sessions"
            ));
        }
        if let Some(instance) = &peers.instance {
            if !valid_peer_name(instance) {
                return Some(format!(
                    "instance `{instance}` may only hold letters, digits, `-`, `_` and `.` (at most 64)"
                ));
            }
        }
        if let Some(reason) = self.peer_tokens_problem(peers) {
            return Some(reason);
        }
        match (&peers.tls_cert, &peers.tls_key) {
            (Some(cert), Some(key)) if cert.trim().is_empty() || key.trim().is_empty() => {
                return Some("tls_cert and tls_key must not be empty".to_owned());
            }
            (Some(_), None) | (None, Some(_)) => {
                return Some("tls_cert and tls_key go together: set both or neither".to_owned());
            }
            _ => {}
        }
        for (name, peer) in &peers.peers {
            if let Some(reason) = peer_entry_problem(name, peer) {
                return Some(reason);
            }
        }
        if !peers.publishing() {
            return None;
        }
        let Some(listen) = &peers.listen else {
            return Some("publish needs a listen address".to_owned());
        };
        let Ok(addr) = listen.parse::<std::net::SocketAddr>() else {
            return Some(format!("listen `{listen}` is not an IP:port address"));
        };
        let others = [
            Some(self.listen.as_str()),
            self.admin.as_ref().map(|a| a.listen.as_str()),
            self.tunnel.as_ref().map(|t| t.listen.as_str()),
        ];
        if others
            .into_iter()
            .flatten()
            .any(|other| same_listener(other, addr))
        {
            return Some(
                "listen must differ from the client, admin and tunnel listen addresses".to_owned(),
            );
        }
        if peers.tokens.is_empty() {
            return Some("publish needs peer_token_sha256 or a [[peers.tokens]] entry".to_owned());
        }
        if !addr.ip().is_loopback() && peers.tls_files().is_none() {
            return Some(format!(
                "listen `{listen}` is not a loopback address; without tls_cert and tls_key the peer listener serves plain HTTP, so it may only listen on loopback"
            ));
        }
        None
    }

    /// Why the peer tokens cannot be used: a bad or repeated name, a hash of the wrong shape, one
    /// shared with a stronger credential, or two tokens with the same hash.
    fn peer_tokens_problem(&self, peers: &PeersConfig) -> Option<String> {
        let mut seen: Vec<(&str, String)> = Vec::new();
        for (index, token) in peers.tokens.iter().enumerate() {
            let name = token.name.as_str();
            // The shorthand is named by the key the operator wrote.
            let what = if index == 0 && peers.peer_token_sha256.is_some() {
                "peer_token_sha256".to_owned()
            } else {
                format!("peer token `{name}`")
            };
            if !valid_peer_token_name(name) {
                return Some(format!(
                    "peer token name `{name}` may only hold lowercase letters, digits and `-` (at most 32)"
                ));
            }
            if !is_sha256_hex(&token.token_sha256) {
                return Some(format!("{what} is not 64 hex characters"));
            }
            let hash = token.token_sha256.to_ascii_lowercase();
            if self
                .admin
                .as_ref()
                .is_some_and(|admin| admin.token_sha256.to_ascii_lowercase() == hash)
            {
                return Some(format!("{what} is the same as the admin token"));
            }
            if let Some((stream, _)) = self
                .streams
                .iter()
                .find(|(_, stream)| stream.token_sha256.to_ascii_lowercase() == hash)
            {
                return Some(format!(
                    "{what} is the same as the token of stream `{stream}`"
                ));
            }
            if seen.iter().any(|(other, _)| *other == name) {
                return Some(format!("peer token name `{name}` is used twice"));
            }
            if let Some((other, _)) = seen.iter().find(|(_, other)| *other == hash) {
                return Some(format!("{what} is the same as peer token `{other}`"));
            }
            seen.push((name, hash));
        }
        None
    }
}

/// True when the configured address `other` would bind the same socket as `addr`; port 0 asks
/// the system for a free port, so two of those never collide.
fn same_listener(other: &str, addr: std::net::SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match other.parse::<std::net::SocketAddr>() {
        Ok(other) => {
            other.port() == addr.port()
                && (other.ip() == addr.ip()
                    || other.ip().is_unspecified()
                    || addr.ip().is_unspecified())
        }
        Err(_) => {
            other
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok())
                == Some(addr.port())
        }
    }
}

fn valid_peer_token_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_peer_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// What is wrong with one `[peers.<name>]`, if anything.
fn peer_entry_problem(name: &str, peer: &PeerEntry) -> Option<String> {
    if !valid_peer_name(name) {
        return Some(format!(
            "peer name `{name}` may only hold letters, digits, `-`, `_` and `.` (at most 64)"
        ));
    }
    let url = peer.url.as_str();
    let https = url.starts_with("https://");
    if !https && !is_loopback_http(url) {
        return Some(format!(
            "[peers.{name}] url must use https, or http on a loopback address"
        ));
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Some(format!("[peers.{name}] url `{url}` is not a URL"));
    };
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.host_str().is_none_or(str::is_empty)
    {
        return Some(format!(
            "[peers.{name}] url must be a plain base URL without credentials, query or fragment"
        ));
    }
    if peer.token_file.trim().is_empty() {
        return Some(format!("[peers.{name}] token_file must not be empty"));
    }
    if https && peer.ca_file.as_deref().is_none_or(|f| f.trim().is_empty()) {
        return Some(format!(
            "[peers.{name}] needs ca_file: an https peer is trusted only through its own CA"
        ));
    }
    None
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// True for an `http://` URL whose host is exactly a loopback name. A prefix
/// match would also accept `http://localhost.example` or `http://127.0.0.1@evil`.
fn is_loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((addr, tail)) if tail.is_empty() || tail.starts_with(':') => {
                return addr == "::1" && tail.get(1..).is_none_or(is_port);
            }
            _ => return false,
        },
        None => match authority.split_once(':') {
            Some((host, port)) if is_port(port) => host,
            Some(_) => return false,
            None => authority,
        },
    };
    host == "127.0.0.1" || host == "localhost"
}

fn is_port(text: &str) -> bool {
    !text.is_empty() && text.len() <= 5 && text.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_auth_backoff_defaults_and_validates() {
        let config = Config::parse("").unwrap();
        assert!(config.auth.backoff);
        assert_eq!(config.auth.max_failures, 10);
        assert_eq!(config.auth.window_secs, 60);

        let config =
            Config::parse("[auth]\nbackoff = false\nmax_failures = 3\nwindow_secs = 30\n").unwrap();
        assert!(!config.auth.backoff);
        assert_eq!(config.auth.max_failures, 3);
        assert_eq!(config.auth.window_secs, 30);

        for bad in [
            "[auth]\nmax_failures = 0\n",
            "[auth]\nmax_failures = 10001\n",
            "[auth]\nwindow_secs = 0\n",
            "[auth]\nwindow_secs = 86401\n",
            "[auth]\nbogus = 1\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_reload_interval_is_checked() {
        let base = "[upstream.zen]\nbase_url = \"https://example.com\"\n";
        assert!(Config::parse(&format!(
            "{base}[reload]\nwatch = true\ninterval_secs = 5\n"
        ))
        .is_ok());
        let zero = Config::parse(&format!("{base}[reload]\ninterval_secs = 0\n")).unwrap_err();
        assert!(matches!(zero, ConfigError::BadReload(_)), "{zero}");
        assert!(Config::parse(&format!("{base}[reload]\ninterval_secs = 99999\n")).is_err());
        assert!(Config::parse(&format!("{base}[reload]\nbogus = 1\n")).is_err());
        let default = Config::parse(base).unwrap();
        assert!(!default.reload.watch && default.reload.interval_secs == 5);
    }

    #[test]
    fn the_log_section_defaults_and_validates() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.log.level, "info");
        assert!(config.log.stderr && config.log.file.is_none());
        let config = Config::parse(
            "[log]\nlevel = \"info,reiki::tunnel=debug\"\nfile = \"/var/log/reiki.log\"\nformat = \"json\"",
        )
        .unwrap();
        assert_eq!(config.log.format, LogFormat::Json);
        for bad in [
            "[log]\nlevel = \"reiki=loud\"",
            "[log]\nfile = \"reiki.log\"",
            "[log]\nstderr = false",
            "[log]\nfile = \"/x\"\nfile_keep = 101",
            "[log]\nformat = \"xml\"",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn base(extra: &str) -> String {
        format!(
            r#"
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"

[keys.zen-b]
upstream = "zen"
file = "/etc/reiki/keys/zen-b"
{extra}
"#
        )
    }

    #[test]
    fn parses_a_minimal_config_with_defaults() {
        let text = base(&format!(
            "[streams.client-a]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\", \"zen-b\"]\n"
        ));
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.listen, "127.0.0.1:4097");
        assert_eq!(config.upstream["zen"].auth_header, "authorization");
        assert_eq!(config.upstream["zen"].auth_format, "Bearer {key}");
        assert_eq!(config.upstream["zen"].max_decoded_bytes, 64 * 1024 * 1024);
        assert_eq!(config.upstream["zen"].response_idle_timeout_secs, 300);
        assert_eq!(config.upstream["zen"].response_timeout_secs, None);
        assert_eq!(config.streams["client-a"].strategy, StrategyConfig::Sticky);
        assert_eq!(config.streams["client-a"].rate_limit_per_minute, None);
    }

    #[test]
    fn an_upstream_body_cap_must_be_at_least_one_byte() {
        let zero = base("").replace(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"",
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\nmax_decoded_bytes = 0",
        );
        let error = Config::parse(&zero).unwrap_err();
        assert!(
            matches!(error, ConfigError::BadUpstream { ref upstream, .. } if upstream == "zen"),
            "{error}"
        );
        let text = base("").replace(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"",
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\nmax_decoded_bytes = 1",
        );
        assert_eq!(
            Config::parse(&text).unwrap().upstream["zen"].max_decoded_bytes,
            1
        );
    }

    #[test]
    fn response_timeouts_must_be_positive() {
        let with = |line: &str| {
            base("").replace(
                "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"",
                &format!("[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n{line}"),
            )
        };
        for line in [
            "response_idle_timeout_secs = 0",
            "response_timeout_secs = 0",
        ] {
            let error = Config::parse(&with(line)).unwrap_err();
            assert!(
                matches!(error, ConfigError::BadUpstream { ref upstream, .. } if upstream == "zen"),
                "{line}: {error}"
            );
        }
        let config = Config::parse(&with(
            "response_idle_timeout_secs = 7\nresponse_timeout_secs = 30",
        ))
        .unwrap();
        assert_eq!(config.upstream["zen"].response_idle_timeout_secs, 7);
        assert_eq!(config.upstream["zen"].response_timeout_secs, Some(30));
    }

    #[test]
    fn parses_a_stream_rate_limit_and_rejects_zero() {
        let limited = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\nrate_limit_per_minute = 60\n"
        ));
        assert_eq!(
            Config::parse(&limited).unwrap().streams["s"].rate_limit_per_minute,
            Some(60)
        );
        let zero = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"zen-a\"]\nrate_limit_per_minute = 0\n"
        ));
        assert_eq!(
            Config::parse(&zero).unwrap_err(),
            ConfigError::InvalidRateLimit("s".into())
        );
    }

    #[test]
    fn rejects_a_key_with_an_unknown_upstream() {
        let text = "[keys.k]\nupstream = \"nope\"\nfile = \"/x\"\n";
        assert_eq!(
            Config::parse(text).unwrap_err(),
            ConfigError::UnknownUpstream {
                key: "k".into(),
                upstream: "nope".into()
            }
        );
    }

    #[test]
    fn rejects_a_chain_with_an_unknown_key() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\", \"ghost\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::UnknownKey {
                stream: "s".into(),
                key: "ghost".into()
            }
        );
    }

    #[test]
    fn rejects_empty_chains_and_bad_hashes() {
        let empty = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = []\n"
        ));
        assert_eq!(
            Config::parse(&empty).unwrap_err(),
            ConfigError::EmptyChain("s".into())
        );
        let bad = base("[streams.s]\ntoken_sha256 = \"short\"\nchain = [\"zen-a\"]\n");
        assert_eq!(
            Config::parse(&bad).unwrap_err(),
            ConfigError::BadTokenHash("s".into())
        );
    }

    #[test]
    fn rejects_two_streams_with_the_same_token() {
        let text = base(&format!(
            "[streams.one]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n[streams.two]\ntoken_sha256 = \"{}\"\nchain = [\"zen-b\"]\n",
            HASH_A.to_uppercase()
        ));
        assert!(matches!(
            Config::parse(&text).unwrap_err(),
            ConfigError::DuplicateToken(..)
        ));
    }

    #[test]
    fn rejects_plain_http_upstreams_except_loopback() {
        let remote = "[upstream.x]\nbase_url = \"http://example.com/v1\"\n";
        assert_eq!(
            Config::parse(remote).unwrap_err(),
            ConfigError::InsecureUpstream("x".into())
        );
        let local = "[upstream.x]\nbase_url = \"http://127.0.0.1:9000\"\n";
        assert!(Config::parse(local).is_ok());
    }

    #[test]
    fn rejects_unknown_fields_so_typos_do_not_pass_silently() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"zen-a\"]\nstrategi = \"sticky\"\n"
        ));
        assert!(matches!(
            Config::parse(&text).unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn round_robin_strategy_parses() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"zen-a\", \"zen-b\"]\nstrategy = \"round_robin\"\n"
        ));
        let config = Config::parse(&text).unwrap();
        assert_eq!(
            Strategy::from(config.streams["s"].strategy),
            Strategy::RoundRobin
        );
    }

    #[test]
    fn admin_is_absent_by_default() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert!(Config::parse(&text).unwrap().admin.is_none());
    }

    #[test]
    fn parses_an_admin_listener() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n[admin]\nlisten = \"127.0.0.1:4098\"\ntoken_sha256 = \"{HASH_B}\"\n"
        ));
        let admin = Config::parse(&text).unwrap().admin.unwrap();
        assert_eq!(admin.listen, "127.0.0.1:4098");
        assert_eq!(admin.token_sha256, HASH_B);
    }

    #[test]
    fn rejects_a_bad_admin_token_hash() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n[admin]\nlisten = \"127.0.0.1:4098\"\ntoken_sha256 = \"nope\"\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadAdminTokenHash
        );
    }

    #[test]
    fn rejects_an_admin_listen_equal_to_the_client_listen() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n[admin]\nlisten = \"127.0.0.1:4097\"\ntoken_sha256 = \"{HASH_B}\"\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::AdminListenConflict
        );
    }

    #[test]
    fn rejects_unknown_fields_in_the_admin_table() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n[admin]\nlisten = \"127.0.0.1:4098\"\ntoken_sha256 = \"{HASH_B}\"\ntoken = \"oops\"\n"
        ));
        assert!(matches!(
            Config::parse(&text).unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn parses_model_aliases_and_drop_params() {
        let text = format!(
            r#"
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
model_aliases = {{ "gpt-4o" = "opencode/gpt-4o" }}
drop_params = ["stream_options", "reasoning_effort"]

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"

[streams.s]
token_sha256 = "{HASH_A}"
chain = ["zen-a"]
"#
        );
        let config = Config::parse(&text).unwrap();
        let upstream = &config.upstream["zen"];
        assert_eq!(upstream.model_aliases["gpt-4o"], "opencode/gpt-4o");
        assert_eq!(upstream.drop_params, ["stream_options", "reasoning_effort"]);
    }

    #[test]
    fn transport_options_default_off_and_parse_when_set() {
        let text = |options: &str| {
            format!(
                r#"
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
{options}

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"

[streams.s]
token_sha256 = "{HASH_A}"
chain = ["zen-a"]
"#
            )
        };
        let plain = Config::parse(&text("")).unwrap();
        assert!(!plain.upstream["zen"].http2);
        assert!(!plain.upstream["zen"].forward_accept_encoding);

        let set = Config::parse(&text("http2 = true\nforward_accept_encoding = true")).unwrap();
        assert!(set.upstream["zen"].http2);
        assert!(set.upstream["zen"].forward_accept_encoding);

        assert!(Config::parse(&text("http2 = \"yes\"")).is_err());
    }

    #[test]
    fn rejects_an_empty_model_alias() {
        for aliases in [r#"{ "" = "x" }"#, r#"{ "gpt-4o" = "" }"#] {
            let text = format!(
                "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\nmodel_aliases = {aliases}\n"
            );
            assert_eq!(
                Config::parse(&text).unwrap_err(),
                ConfigError::EmptyModelAlias("zen".into())
            );
        }
    }

    #[test]
    fn rejects_an_empty_drop_param() {
        let text =
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\ndrop_params = [\"\"]\n";
        assert_eq!(
            Config::parse(text).unwrap_err(),
            ConfigError::EmptyDropParam("zen".into())
        );
    }

    #[test]
    fn parses_a_key_quota_reset_schedule() {
        let daily = Config::parse(&base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        )))
        .unwrap();
        assert_eq!(daily.quota_reset("zen-a"), None, "unset stays unavailable");

        for (value, expected) in [
            (
                "daily 00:00 UTC",
                Reset::Daily {
                    hour: 0,
                    minute: 0,
                    offset_seconds: 0,
                },
            ),
            (
                "monthly 1 03:30 +02:00",
                Reset::Monthly {
                    day: 1,
                    hour: 3,
                    minute: 30,
                    offset_seconds: 7_200,
                },
            ),
        ] {
            let config = Config::parse(&single_key_config(value)).unwrap();
            assert_eq!(config.quota_reset("only"), Some(expected));
        }
    }

    fn single_key_config(value: &str) -> String {
        format!(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n\
             [keys.only]\nupstream = \"zen\"\nfile = \"/x\"\nquota_resets = \"{value}\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"only\"]\n"
        )
    }

    #[test]
    fn rejects_an_invalid_quota_reset_schedule() {
        for value in [
            "hourly 00:00 UTC",
            "daily 24:00 UTC",
            "daily 00:00 +15:00",
            "monthly 0 00:00 UTC",
            "monthly 29 00:00 UTC",
        ] {
            let err = Config::parse(&single_key_config(value)).unwrap_err();
            assert!(
                matches!(err, ConfigError::BadQuotaResets { .. }),
                "`{value}` should be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn reports_which_key_has_a_bad_quota_reset() {
        let err = Config::parse(&single_key_config("nonsense")).unwrap_err();
        assert_eq!(
            err,
            ConfigError::BadQuotaResets {
                key: "only".into(),
                reason: "`nonsense` is not `daily HH:MM <zone>` or `monthly D HH:MM <zone>`".into(),
            }
        );
        assert_eq!(
            err.to_string(),
            "key `only` has an invalid quota_resets: `nonsense` is not `daily HH:MM <zone>` or `monthly D HH:MM <zone>`"
        );
    }

    #[test]
    fn an_unknown_field_beside_quota_resets_is_still_rejected() {
        let text = single_key_config("daily 00:00 UTC") + "quota_reset = \"daily 00:00 UTC\"\n";
        assert!(matches!(
            Config::parse(&text).unwrap_err(),
            ConfigError::Parse(_)
        ));
    }
    #[test]
    fn loopback_http_matches_only_exact_hosts() {
        for ok in [
            "http://127.0.0.1",
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/v1",
            "http://localhost/x",
            "http://localhost:3000",
            "http://[::1]:9000/v1",
            "http://[::1]",
        ] {
            assert!(is_loopback_http(ok), "{ok} should be loopback");
        }
        for bad in [
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://127.0.0.10",
            "http://127.0.0.1@evil.example",
            "http://localhost:80@evil.example/",
            "http://localhost:port",
            "http://[::2]",
            "http://[::1]x",
            "https://localhost",
            "ftp://127.0.0.1",
            "http://",
        ] {
            assert!(!is_loopback_http(bad), "{bad} should not be loopback");
        }
    }

    #[test]
    fn config_rejects_a_lookalike_loopback_upstream() {
        let text = "[upstream.x]\nbase_url = \"http://localhost.evil.example\"\n";
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::InsecureUpstream(_))
        ));
    }

    #[test]
    fn plugin_tables_are_kept_for_the_plugin_to_read() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n\
             [plugins.secrets]\nenabled = false\npath = \"/etc/reiki/secrets.toml\"\n"
        ));
        let config = Config::parse(&text).unwrap();
        let table = &config.plugins["secrets"];
        assert_eq!(
            table.get("enabled").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            table.get("path").and_then(toml::Value::as_str),
            Some("/etc/reiki/secrets.toml")
        );
    }

    #[test]
    fn a_plugin_may_name_configured_streams_and_passthrough_only() {
        let ok = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n\
             [plugins.marker]\nstreams = [\"s\", \"passthrough\"]\n"
        ));
        assert!(Config::parse(&ok).is_ok());
        let typo = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n\
             [plugins.marker]\nstreams = [\"ghost\"]\n"
        ));
        let err = Config::parse(&typo).unwrap_err().to_string();
        assert!(
            err.contains("marker") && err.contains("ghost"),
            "error was: {err}"
        );
    }

    #[test]
    fn a_forward_command_is_kept_on_the_upstream() {
        let text = base(&format!(
            "[upstream.zen.forward]\ncommand = \"python3\"\nargs = [\"tests/external_child.py\"]\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        let config = Config::parse(&text).unwrap();
        let fwd = config.upstream["zen"].forward.as_ref().unwrap();
        assert_eq!(fwd.command.as_deref(), Some("python3"));
        assert_eq!(fwd.args, ["tests/external_child.py"]);
    }

    #[test]
    fn a_forward_empty_or_whitespace_command_is_rejected() {
        let text = base(&format!(
            "[upstream.zen.forward]\ncommand = \"\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        let err = Config::parse(&text).unwrap_err();
        assert_eq!(
            err,
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "command must not be empty".into(),
            }
        );
        assert_eq!(
            err.to_string(),
            "upstream `zen` has an invalid forward: command must not be empty"
        );
        let text = base(&format!(
            "[upstream.zen.forward]\ncommand = \"  \"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        let err = Config::parse(&text).unwrap_err();
        assert_eq!(
            err,
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "command must not be empty".into(),
            }
        );
    }

    #[test]
    fn forward_rejects_incompatible_options() {
        let text = format!(
            r#"
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
http2 = true
forward = {{command = "python3"}}

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"

[streams.s]
token_sha256 = "{HASH_A}"
chain = ["zen-a"]
"#
        );
        let err = Config::parse(&text).unwrap_err();
        assert_eq!(
            err,
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "http2 does not apply to a forward upstream".into(),
            }
        );
        let text = format!(
            r#"
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
title_case_headers = true
forward = {{command = "python3"}}

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"

[streams.s]
token_sha256 = "{HASH_A}"
chain = ["zen-a"]
"#
        );
        let err = Config::parse(&text).unwrap_err();
        assert_eq!(
            err,
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "title_case_headers does not apply to a forward upstream".into(),
            }
        );
        // forward on an intercept host
        let text = format!(
            r#"
[tunnel]
listen = "127.0.0.1:8443"
allow = ["example.com"]
connect_timeout_secs = 10
[tunnel.intercept]
ca_cert = "/x/ca.crt"
ca_key = "/x/ca.key"
hosts = [{{host="example.com", upstream="zen", stream="s"}}]

[upstream.zen]
base_url = "https://example.com"
forward = {{command = "python3"}}

[keys.k]
upstream = "zen"
file = "/x"

[streams.s]
token_sha256 = "{HASH_A}"
chain = ["k"]
"#
        );
        Config::parse(&text).expect("a forward upstream may serve an intercepted host");
    }

    #[test]
    fn forward_validates_exactly_one_of_command_or_socket() {
        let text = base(&format!(
            "[upstream.zen.forward]\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "set exactly one of `command` or `socket`".into(),
            }
        );
        let text = base(&format!(
            "[upstream.zen.forward]\ncommand = \"python3\"\nsocket = \"/tmp/sock\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "set exactly one of `command` or `socket`".into(),
            }
        );
    }

    #[test]
    fn forward_args_pass_env_apply_only_to_command() {
        let text = base(&format!(
            "[upstream.zen.forward]\nsocket = \"/tmp/sock\"\nargs = [\"x\"]\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "`args` and `pass_env` apply only to `command`".into(),
            }
        );
        let text = base(&format!(
            "[upstream.zen.forward]\nsocket = \"/tmp/sock\"\npass_env = [\"X\"]\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "`args` and `pass_env` apply only to `command`".into(),
            }
        );
    }

    #[test]
    fn forward_expect_uid_applies_only_to_socket() {
        let text = base(&format!(
            "[upstream.zen.forward]\ncommand = \"python3\"\nexpect_uid = 1000\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "`expect_uid` applies only to `socket`".into(),
            }
        );
    }

    #[test]
    fn forward_socket_must_be_absolute_and_short_enough() {
        let text = base(&format!(
            "[upstream.zen.forward]\nsocket = \"tmp/sock\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "socket must be an absolute path".into(),
            }
        );
        let edge = format!("/{}", "a".repeat(102));
        let text = base(&format!(
            "[upstream.zen.forward]\nsocket = \"{edge}\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert!(Config::parse(&text).is_ok(), "103 bytes is allowed");
        let long = format!("/{}", "a".repeat(103));
        let text = base(&format!(
            "[upstream.zen.forward]\nsocket = \"{}\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n",
            long
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "socket path must be at most 103 bytes".into(),
            }
        );
    }

    #[test]
    fn a_passthrough_forward_upstream_may_serve_an_intercepted_host() {
        let text = r#"
[tunnel]
listen = "127.0.0.1:8443"
allow = ["example.com"]
connect_timeout_secs = 10
[tunnel.intercept]
ca_cert = "/x/ca.crt"
ca_key = "/x/ca.key"
hosts = [{host="example.com", upstream="zen"}]

[upstream.zen]
base_url = "https://example.com"
passthrough = true

[upstream.zen.forward]
socket = "/run/forwarder.sock"
"#;
        Config::parse(text).expect("a passthrough forward upstream may serve an intercepted host");
    }

    #[test]
    fn forward_socket_needs_unix() {
        if cfg!(unix) {
            return;
        }
        let text = base(&format!(
            "[upstream.zen.forward]\nsocket = \"/tmp/sock\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::BadForward {
                upstream: "zen".into(),
                reason: "socket forwarding needs a Unix platform".into(),
            }
        );
    }

    #[test]
    fn the_admin_token_must_differ_from_every_stream_token() {
        let text = base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n[admin]\nlisten = \"127.0.0.1:4098\"\ntoken_sha256 = \"{}\"\n",
            HASH_A.to_ascii_uppercase()
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::AdminTokenReused("s".into())
        );
    }

    #[test]
    fn a_stream_cannot_take_the_passthrough_name() {
        let text = base(&format!(
            "[streams.passthrough]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
        ));
        assert_eq!(
            Config::parse(&text).unwrap_err(),
            ConfigError::ReservedStreamName("passthrough".into())
        );
    }

    #[test]
    fn forward_max_concurrent_needs_a_command_and_a_positive_value() {
        for (extra, reason) in [
            (
                "socket = \"/run/x.sock\"\nmax_concurrent = 4",
                "`max_concurrent` applies only to `command`",
            ),
            (
                "command = \"/bin/x\"\nmax_concurrent = 0",
                "`max_concurrent` must be at least 1",
            ),
        ] {
            let text = base(&format!(
                "[upstream.zen.forward]\n{extra}\n\
                 [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n"
            ));
            assert_eq!(
                Config::parse(&text).unwrap_err(),
                ConfigError::BadForward {
                    upstream: "zen".into(),
                    reason: reason.into(),
                }
            );
        }
    }

    #[cfg(feature = "peers")]
    const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn with_peers(peers: &str) -> String {
        base(&format!(
            "[streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"zen-a\"]\n\
             [admin]\nlisten = \"127.0.0.1:4098\"\ntoken_sha256 = \"{HASH_B}\"\n{peers}"
        ))
    }

    fn peers_error(peers: &str) -> String {
        match Config::parse(&with_peers(peers)) {
            Err(ConfigError::BadPeers(reason)) => reason,
            other => panic!("expected a [peers] error for {peers:?}, got {other:?}"),
        }
    }

    #[cfg(feature = "peers")]
    #[test]
    fn peers_are_absent_by_default_and_off_when_present() {
        assert!(Config::parse(&with_peers("")).unwrap().peers.is_none());
        let peers = Config::parse(&with_peers("[peers]\n"))
            .unwrap()
            .peers
            .unwrap();
        assert!(!peers.enabled && !peers.publish && !peers.expose_key_labels);
        assert!(!peers.publishing());
        assert_eq!(peers.poll_interval_secs, 30);
        assert!(peers.peers.is_empty() && peers.listen.is_none());
        assert!(!peers.expose_host_stats);
        assert_eq!(peers.session_header, "x-opencode-session");
        assert_eq!(peers.session_window_secs, 300);
        assert_eq!(peers.stream_interval_secs, 5);
        assert_eq!(peers.max_subscribers, 16);
        assert_eq!(peers.max_subscribers_per_peer, 4);
        assert_eq!(peers.cooldown_bucket_secs, 300);
        assert!(Config::parse(&with_peers("[peers]\ncooldown_bucket_secs = 0\n")).is_ok());
        assert!(
            peers_error("[peers]\ncooldown_bucket_secs = 86401\n").contains("cooldown_bucket_secs")
        );
        assert_eq!(
            PeersConfig::check_line(Some(&peers)),
            "peers: disabled",
            "a present but disabled table is reported as disabled"
        );
        assert_eq!(PeersConfig::check_line(None), "peers: disabled");
    }

    #[cfg(feature = "peers")]
    #[test]
    fn peers_parse_settings_and_peer_tables() {
        let config = Config::parse(&with_peers(&format!(
            "[peers]\nenabled = true\npublish = true\nlisten = \"127.0.0.1:4099\"\n\
             peer_token_sha256 = \"{HASH_C}\"\npoll_interval_secs = 10\nexpose_key_labels = true\n\
             [peers.ct-4]\nurl = \"https://10.0.6.153:4099\"\ntoken_file = \"/etc/reiki/peers/ct-4.token\"\n\
             ca_file = \"/etc/reiki/peers/ca.pem\"\n\
             [peers.local]\nurl = \"http://127.0.0.1:5099\"\ntoken_file = \"/t\"\n"
        )))
        .unwrap();
        let peers = config.peers.unwrap();
        assert!(peers.publishing() && peers.expose_key_labels);
        assert_eq!(peers.poll_interval_secs, 10);
        assert_eq!(peers.peers.len(), 2);
        assert_eq!(peers.peers["ct-4"].url, "https://10.0.6.153:4099");
        assert_eq!(peers.peers["local"].ca_file, None);
        assert_eq!(
            PeersConfig::check_line(Some(&peers)),
            "peers: enabled, publishing on 127.0.0.1:4099 (key labels shown), 2 peer(s)[ct-4, local], poll every 10s"
        );
    }

    #[cfg(feature = "peers")]
    #[test]
    fn peers_refuse_unknown_fields_in_settings_and_peer_tables() {
        for bad in [
            "[peers]\nenable = true\n",
            "[peers.x]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"/t\"\ntoken = \"oops\"\n",
            "[peers.x]\ntoken_file = \"/t\"\n",
        ] {
            assert!(
                matches!(Config::parse(&with_peers(bad)), Err(ConfigError::Parse(_))),
                "{bad}"
            );
        }
    }

    #[cfg(feature = "peers")]
    #[test]
    fn publishing_needs_a_separate_loopback_listener_and_a_distinct_token() {
        let publish = |listen: &str, hash: &str| {
            format!(
                "[peers]\nenabled = true\npublish = true\nlisten = \"{listen}\"\npeer_token_sha256 = \"{hash}\"\n"
            )
        };
        assert!(Config::parse(&with_peers(&publish("127.0.0.1:4099", HASH_C))).is_ok());
        assert!(
            Config::parse(&with_peers(&publish("127.0.0.1:0", HASH_C))).is_ok(),
            "port 0 is a fresh port"
        );
        for (peers, needle) in [
            (
                "[peers]\nenabled = true\npublish = true\n".to_owned(),
                "listen address",
            ),
            (
                "[peers]\nenabled = true\npublish = true\nlisten = \"127.0.0.1:4099\"\n".to_owned(),
                "peer_token_sha256",
            ),
            (publish("127.0.0.1:4097", HASH_C), "must differ"),
            (publish("127.0.0.1:4098", HASH_C), "must differ"),
            (publish("0.0.0.0:4098", HASH_C), "must differ"),
            (publish("localhost:4099", HASH_C), "IP:port"),
            (publish("10.0.0.5:4099", HASH_C), "loopback"),
            (publish("127.0.0.1:4099", "short"), "64 hex"),
            (publish("127.0.0.1:4099", HASH_B), "admin token"),
            (
                publish("127.0.0.1:4099", &HASH_A.to_ascii_uppercase()),
                "stream `s`",
            ),
            (
                format!("[peers]\npeer_token_sha256 = \"{HASH_B}\"\n"),
                "admin token",
            ),
            (
                "[peers]\npoll_interval_secs = 0\n".to_owned(),
                "poll_interval_secs",
            ),
            (
                "[peers]\nsession_window_secs = 5\n".to_owned(),
                "session_window_secs",
            ),
            (
                "[peers]\nsession_header = \"X-Session\"\n".to_owned(),
                "lowercase",
            ),
            (
                "[peers]\nsession_header = \"bad header\"\n".to_owned(),
                "lowercase",
            ),
            (
                "[peers]\nsession_header = \"authorization\"\n".to_owned(),
                "credentials",
            ),
            ("[peers]\ninstance = \"a b\"\n".to_owned(), "instance"),
            (
                "[peers]\nstream_interval_secs = 0\n".to_owned(),
                "stream_interval_secs",
            ),
            (
                "[peers]\nmax_subscribers = 0\n".to_owned(),
                "max_subscribers",
            ),
            (
                "[peers]\nmax_subscribers = 2\nmax_subscribers_per_peer = 3\n".to_owned(),
                "max_subscribers_per_peer",
            ),
        ] {
            let reason = peers_error(&peers);
            assert!(reason.contains(needle), "{peers}: {reason}");
        }
        assert!(
            Config::parse(&with_peers("[peers]\npublish = true\n")).is_ok(),
            "publish without enabled does nothing and needs nothing"
        );
    }

    #[cfg(feature = "peers")]
    const HASH_D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    #[cfg(feature = "peers")]
    #[test]
    fn several_named_peer_tokens_join_the_shorthand() {
        let config = Config::parse(&with_peers(&format!(
            "[peers]\nenabled = true\npublish = true\nlisten = \"127.0.0.1:4099\"\n\
             peer_token_sha256 = \"{HASH_C}\"\nmax_subscribers_per_token = 3\n\
             [[peers.tokens]]\nname = \"ct-5\"\ntoken_sha256 = \"{HASH_D}\"\n"
        )))
        .unwrap();
        let peers = config.peers.unwrap();
        let names: Vec<&str> = peers.tokens.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["default", "ct-5"]);
        assert_eq!(peers.tokens[1].token_sha256, HASH_D);
        assert_eq!(peers.max_subscribers_per_token, 3);
        assert_eq!(
            PeersConfig::check_line(Some(&peers)),
            "peers: enabled, publishing on 127.0.0.1:4099 (key labels hidden), 2 token(s)[default, ct-5], 0 peer(s)[], poll every 30s"
        );
        let only_list = Config::parse(&with_peers(&format!(
            "[peers]\nenabled = true\npublish = true\nlisten = \"127.0.0.1:4099\"\n\
             [[peers.tokens]]\nname = \"ct-5\"\ntoken_sha256 = \"{HASH_D}\"\n"
        )))
        .unwrap();
        assert_eq!(
            only_list.peers.unwrap().tokens.len(),
            1,
            "the list alone is enough"
        );
        assert_eq!(
            Config::parse(&with_peers("[peers]\n"))
                .unwrap()
                .peers
                .unwrap()
                .max_subscribers_per_token,
            8
        );
    }

    #[cfg(feature = "peers")]
    #[test]
    fn peer_tokens_need_unique_names_and_hashes_apart_from_every_other_credential() {
        let token = |name: &str, hash: &str| {
            format!("[[peers.tokens]]\nname = \"{name}\"\ntoken_sha256 = \"{hash}\"\n")
        };
        for (peers, needle) in [
            (
                format!("[peers]\n{}{}", token("a", HASH_C), token("a", HASH_D)),
                "`a` is used twice",
            ),
            (
                format!(
                    "[peers]\npeer_token_sha256 = \"{HASH_C}\"\n{}",
                    token("default", HASH_D)
                ),
                "`default` is used twice",
            ),
            (
                format!("[peers]\n{}{}", token("a", HASH_C), token("b", HASH_C)),
                "peer token `b` is the same as peer token `a`",
            ),
            (
                format!(
                    "[peers]\npeer_token_sha256 = \"{HASH_C}\"\n{}",
                    token("b", &HASH_C.to_ascii_uppercase())
                ),
                "the same as peer token `default`",
            ),
            (
                format!("[peers]\n{}", token("a", HASH_B)),
                "peer token `a` is the same as the admin token",
            ),
            (
                format!("[peers]\n{}", token("a", HASH_A)),
                "peer token `a` is the same as the token of stream `s`",
            ),
            (format!("[peers]\n{}", token("a", "short")), "64 hex"),
            (format!("[peers]\n{}", token("Upper", HASH_C)), "token name"),
            (format!("[peers]\n{}", token("a_b", HASH_C)), "token name"),
            (format!("[peers]\n{}", token("", HASH_C)), "token name"),
            (
                format!("[peers]\n{}", token(&"a".repeat(33), HASH_C)),
                "token name",
            ),
            (
                "[peers]\nmax_subscribers_per_token = 0\n".to_owned(),
                "max_subscribers_per_token",
            ),
        ] {
            let reason = peers_error(&peers);
            assert!(reason.contains(needle), "{peers}: {reason}");
        }
        assert!(Config::parse(&with_peers(&format!(
            "[peers]\n{}",
            token(&"a".repeat(32), HASH_C)
        )))
        .is_ok());
        assert!(matches!(
            Config::parse(&with_peers(&format!(
                "[peers]\n[[peers.tokens]]\nname = \"a\"\ntoken_sha256 = \"{HASH_C}\"\ntoken = \"x\"\n"
            ))),
            Err(ConfigError::Parse(_))
        ));
    }

    #[cfg(feature = "peers")]
    #[test]
    fn peer_tables_need_a_safe_url_a_token_file_and_a_ca_for_https() {
        let peer = |name: &str, body: &str| format!("[peers.{name}]\n{body}\n");
        assert!(Config::parse(&with_peers(&peer(
            "a",
            "url = \"https://peer.example:4099\"\ntoken_file = \"/t\"\nca_file = \"/ca.pem\""
        )))
        .is_ok());
        for (peers, needle) in [
            (
                peer("a", "url = \"http://10.0.0.5:4099\"\ntoken_file = \"/t\""),
                "https",
            ),
            (
                peer("a", "url = \"https://peer.example\"\ntoken_file = \"/t\""),
                "ca_file",
            ),
            (
                peer(
                    "a",
                    "url = \"https://u:p@peer.example\"\ntoken_file = \"/t\"\nca_file = \"/c\"",
                ),
                "credentials",
            ),
            (
                peer(
                    "a",
                    "url = \"https://peer.example/?x=1\"\ntoken_file = \"/t\"\nca_file = \"/c\"",
                ),
                "query",
            ),
            (
                peer("a", "url = \"http://127.0.0.1:1\"\ntoken_file = \" \""),
                "token_file",
            ),
            (
                peer(
                    "\"a b\"",
                    "url = \"http://127.0.0.1:1\"\ntoken_file = \"/t\"",
                ),
                "peer name",
            ),
        ] {
            let reason = peers_error(&peers);
            assert!(reason.contains(needle), "{peers}: {reason}");
        }
    }

    #[cfg(not(feature = "peers"))]
    #[test]
    fn a_build_without_the_feature_refuses_a_peers_table() {
        assert!(peers_error("[peers]\n").contains("built without"));
    }

    #[test]
    fn an_old_plugin_name_is_renamed_and_never_doubled() {
        let old = Config::parse(&base("[plugins.gov]\nmode = \"report\"\n")).unwrap();
        assert!(old.plugins.contains_key("controlled") && !old.plugins.contains_key("gov"));
        let both = base("[plugins.gov]\n[plugins.controlled]\n");
        let err = Config::parse(&both).unwrap_err().to_string();
        assert!(err.contains("gov") && err.contains("controlled"), "{err}");
    }
}
