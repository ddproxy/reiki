use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::chain::Strategy;
use crate::quota::Reset;

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    UnknownUpstream { key: String, upstream: String },
    UnknownKey { stream: String, key: String },
    EmptyChain(String),
    BadTokenHash(String),
    DuplicateToken(String, String),
    InsecureUpstream(String),
    InvalidRateLimit(String),
    BadQuotaResets { key: String, reason: String },
    EmptyModelAlias(String),
    EmptyDropParam(String),
    BadAdminTokenHash,
    AdminListenConflict,
    UnknownPlugin { name: String, known: Vec<String> },
    PluginConfig { name: String, message: String },
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
            Self::BadAdminTokenHash => {
                write!(
                    f,
                    "admin token_sha256 is not 64 hex characters (it must not be empty)"
                )
            }
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
    #[serde(skip)]
    quota_resets: BTreeMap<String, Reset>,
    #[serde(default)]
    pub plugins: BTreeMap<String, toml::Table>,
}

fn default_listen() -> String {
    "127.0.0.1:4097".to_owned()
}

#[derive(Debug, Deserialize)]
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
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
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
        }
        for (name, key) in &self.keys {
            if !self.upstream.contains_key(&key.upstream) {
                return Err(ConfigError::UnknownUpstream {
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
}
