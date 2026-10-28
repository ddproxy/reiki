use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::chain::Strategy;
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
    AdminListenConflict,
    BadTunnel(String),
    BadLog(String),
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
            Self::BadTunnel(reason) => write!(f, "invalid [tunnel]: {reason}"),
            Self::BadLog(reason) => write!(f, "invalid [log]: {reason}"),
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
    #[serde(skip)]
    quota_resets: BTreeMap<String, Reset>,
    #[serde(default)]
    pub plugins: BTreeMap<String, toml::Table>,
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
    /// Send header names in Title-Case (`Content-Type`) on HTTP/1.1. Ignored with `http2`.
    #[serde(default)]
    pub title_case_headers: bool,
    /// Send `Connection: keep-alive` explicitly; HTTP/1.1 connections persist without it.
    #[serde(default)]
    pub send_keep_alive: bool,
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
    pub hosts: Vec<InterceptHost>,
}

fn default_leaf_ttl_hours() -> u64 {
    24
}

/// One intercepted host and the upstream its requests are relayed to.
#[derive(Debug, Deserialize, Clone)]
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
        config.resolve_quota_resets()?;
        config.validate()?;
        Ok(config)
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
            if upstream.forward.is_some() {
                return Some("an intercepted host cannot use a forward upstream".to_owned());
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
        if let Some(admin) = &self.admin {
            if !is_sha256_hex(&admin.token_sha256) {
                return Err(ConfigError::BadAdminTokenHash);
            }
            if admin.listen == self.listen {
                return Err(ConfigError::AdminListenConflict);
            }
        }
        Ok(())
    }
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
        assert_eq!(config.streams["client-a"].strategy, StrategyConfig::Sticky);
        assert_eq!(config.streams["client-a"].rate_limit_per_minute, None);
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
        let err = Config::parse(&text).unwrap_err();
        assert!(matches!(err, ConfigError::BadTunnel(_)));
        assert!(err.to_string().contains("forward upstream"));
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
    fn a_passthrough_forward_upstream_is_refused_on_an_intercepted_host_too() {
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
        let err = Config::parse(text).unwrap_err();
        assert!(matches!(err, ConfigError::BadTunnel(_)), "{err}");
        assert!(err.to_string().contains("forward upstream"));
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
}
