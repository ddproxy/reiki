use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::chain::Strategy;

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
        let config: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, upstream) in &self.upstream {
            // Loopback http is allowed so tests and local mocks work.
            let local = upstream.base_url.starts_with("http://127.0.0.1")
                || upstream.base_url.starts_with("http://localhost");
            if !upstream.base_url.starts_with("https://") && !local {
                return Err(ConfigError::InsecureUpstream(name.clone()));
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
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
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
        Ok(())
    }
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
}
