use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

/// Failed logins one address may make inside `FAIL_WINDOW` before it is refused.
const MAX_FAILURES: u32 = 10;
const FAIL_WINDOW: Duration = Duration::from_secs(60);
/// Addresses tracked at once; past this, expired entries go first and a new address is not tracked.
const MAX_TRACKED: usize = 4096;

/// The peer address of the connection a request arrived on, set by the listener.
#[derive(Debug, Clone, Copy)]
pub struct Peer(pub IpAddr);

/// Limits for a [`LoginThrottle`], from the `[auth]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleSettings {
    pub enabled: bool,
    pub max_failures: u32,
    pub window: Duration,
}

impl Default for ThrottleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_failures: MAX_FAILURES,
            window: FAIL_WINDOW,
        }
    }
}

impl From<&crate::config::AuthConfig> for ThrottleSettings {
    fn from(config: &crate::config::AuthConfig) -> Self {
        Self {
            enabled: config.backoff,
            max_failures: config.max_failures,
            window: Duration::from_secs(config.window_secs),
        }
    }
}

/// Tokens shorter than this are rejected by `reiki hash-token`. A hash cannot reveal how
/// long the token behind it was, so config load can only check the hash shape (64 hex
/// characters); keep tokens at least this long.
pub const MIN_TOKEN_LEN: usize = 16;

/// Counts failed token checks per address so token guessing is slowed and visible.
pub struct LoginThrottle {
    seen: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    /// Replaced on a reload so a new `[auth]` setting applies without a restart.
    settings: RwLock<ThrottleSettings>,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new(ThrottleSettings::default())
    }
}

impl LoginThrottle {
    pub fn new(settings: ThrottleSettings) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            settings: RwLock::new(settings),
        }
    }

    /// Apply a reloaded `[auth]` table. Counts already collected are kept.
    pub fn configure(&self, settings: ThrottleSettings) {
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = settings;
    }

    fn settings(&self) -> ThrottleSettings {
        *self.settings.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// How long `ip` must wait, or `None` when it may try.
    pub fn blocked(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let settings = self.settings();
        if !settings.enabled {
            return None;
        }
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let (count, since) = seen.get(&ip)?;
        let elapsed = now.saturating_duration_since(*since);
        if elapsed >= settings.window {
            return None;
        }
        (*count >= settings.max_failures).then(|| settings.window - elapsed)
    }

    /// Records a failure and returns how many the address has inside the window.
    pub fn fail(&self, ip: IpAddr, now: Instant) -> u32 {
        let settings = self.settings();
        if !settings.enabled {
            return 0;
        }
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= MAX_TRACKED && !seen.contains_key(&ip) {
            seen.retain(|_, (_, since)| now.saturating_duration_since(*since) < settings.window);
            if seen.len() >= MAX_TRACKED {
                // Still full of live entries: drop the least-failed, then oldest, so a flood of fresh
                // addresses cannot leave a new one untracked and does not release a blocked one.
                if let Some(oldest) = seen
                    .iter()
                    .min_by_key(|(_, (count, since))| (*count, *since))
                    .map(|(ip, _)| *ip)
                {
                    seen.remove(&oldest);
                }
            }
        }
        let entry = seen.entry(ip).or_insert((0, now));
        if now.saturating_duration_since(entry.1) >= settings.window {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.0
    }

    /// A good login clears the address's count.
    pub fn succeed(&self, ip: IpAddr) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.remove(&ip);
    }
}

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hashes a token for `token_sha256`, refusing one shorter than [`MIN_TOKEN_LEN`].
pub fn hash_token(token: &str) -> Result<String, String> {
    let token = token.trim();
    let length = token.chars().count();
    if length < MIN_TOKEN_LEN {
        return Err(format!(
            "token must be at least {MIN_TOKEN_LEN} characters (got {length})"
        ));
    }
    Ok(sha256_hex(token))
}

pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The token in a `Proxy-Authorization` value: a bearer token, or the password of a basic
/// credential (the user name when the password is empty), as `HTTPS_PROXY=http://user:token@host` sends it.
pub fn proxy_token(value: &str) -> Option<String> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    let rest = rest.trim();
    let token = if scheme.eq_ignore_ascii_case("bearer") {
        rest.to_owned()
    } else if scheme.eq_ignore_ascii_case("basic") {
        let decoded = String::from_utf8(base64_decode(rest)?).ok()?;
        let (user, pass) = decoded.split_once(':').unwrap_or((decoded.as_str(), ""));
        if pass.is_empty() { user } else { pass }.to_owned()
    } else {
        return None;
    };
    (!token.is_empty()).then_some(token)
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in text.trim_end_matches('=').bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

pub const CREDENTIAL_HEADERS: [&str; 2] = ["x-api-key", "x-goog-api-key"];

pub fn extract_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    token_from_headers(headers).or_else(|| query.and_then(token_from_query))
}

fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let value = value.trim();
        if value.len() > 7 && value[..7].eq_ignore_ascii_case("bearer ") {
            return Some(value[7..].trim().to_owned());
        }
    }
    CREDENTIAL_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

fn param_name(part: &str) -> &str {
    part.split_once('=').map_or(part, |(name, _)| name)
}

fn token_from_query(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            (name == "key" && !value.is_empty()).then(|| value.to_owned())
        })
        .next()
}

// Drops every `key` parameter. Other parameters keep their order and their raw
// percent-encoding, since re-encoding them could change what the upstream receives.
pub fn strip_query_key(query: &str) -> Option<String> {
    let kept = query
        .split('&')
        .filter(|part| param_name(part) != "key")
        .collect::<Vec<_>>()
        .join("&");
    (!kept.is_empty()).then_some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn repeated_failures_block_an_address_until_the_window_passes() {
        let throttle = LoginThrottle::default();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let other: IpAddr = "192.0.2.2".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            throttle.fail(ip, t0);
        }
        assert!(throttle.blocked(ip, t0).is_none());
        throttle.fail(ip, t0);
        assert!(throttle.blocked(ip, t0).is_some());
        assert!(throttle.blocked(other, t0).is_none());
        assert!(throttle.blocked(ip, t0 + FAIL_WINDOW).is_none());
        throttle.succeed(ip);
        assert!(throttle.blocked(ip, t0).is_none());
    }

    #[test]
    fn configured_limits_replace_the_defaults() {
        let settings = ThrottleSettings {
            enabled: true,
            max_failures: 3,
            window: Duration::from_secs(5),
        };
        let throttle = LoginThrottle::new(settings);
        let ip: IpAddr = "192.0.2.9".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..2 {
            throttle.fail(ip, t0);
        }
        assert!(throttle.blocked(ip, t0).is_none());
        throttle.fail(ip, t0);
        assert_eq!(
            throttle.blocked(ip, t0),
            Some(Duration::from_secs(5)),
            "a fresh block lasts the configured window"
        );
        assert!(throttle.blocked(ip, t0 + Duration::from_secs(5)).is_none());

        throttle.configure(ThrottleSettings {
            enabled: true,
            max_failures: 1,
            window: Duration::from_secs(10),
        });
        assert!(
            throttle.blocked(ip, t0).is_some(),
            "reconfigured limits apply to existing counts"
        );
    }

    #[test]
    fn a_disabled_backoff_records_and_blocks_nothing() {
        let throttle = LoginThrottle::new(ThrottleSettings {
            enabled: false,
            max_failures: 1,
            window: Duration::from_secs(60),
        });
        let ip: IpAddr = "192.0.2.10".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..5 {
            assert_eq!(throttle.fail(ip, t0), 0);
        }
        assert!(throttle.blocked(ip, t0).is_none());
    }

    #[test]
    fn a_full_table_evicts_the_oldest_address_instead_of_ignoring_new_ones() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        for n in 0..MAX_TRACKED as u32 {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(n + 1));
            throttle.fail(ip, t0 + Duration::from_millis(u64::from(n)));
        }
        let newcomer: IpAddr = "203.0.113.7".parse().unwrap();
        let now = t0 + Duration::from_secs(1);
        assert_eq!(
            throttle.fail(newcomer, now),
            1,
            "the new address is tracked"
        );
        for _ in 1..MAX_FAILURES {
            throttle.fail(newcomer, now);
        }
        assert!(throttle.blocked(newcomer, now).is_some());
        assert!(
            throttle.seen.lock().unwrap().len() <= MAX_TRACKED,
            "the table stays bounded"
        );
        let first: IpAddr = IpAddr::from(std::net::Ipv4Addr::from(1u32));
        assert!(
            !throttle.seen.lock().unwrap().contains_key(&first),
            "the oldest entry was evicted"
        );
    }

    #[test]
    fn hashes_match_the_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hash_token_refuses_short_tokens() {
        let err = hash_token("shrt").unwrap_err();
        assert!(err.contains("at least"), "{err}");
        assert!(hash_token("a-sixteen-plus-char-token").is_ok());
        let hash = hash_token("a-sixteen-plus-char-token").unwrap();
        assert_eq!(hash, sha256_hex("a-sixteen-plus-char-token"));
    }

    #[test]
    fn constant_time_eq_compares_values() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }

    #[test]
    fn token_comes_from_bearer_or_api_key() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer tok-1"));
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-1"));
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("bearer   tok-2 "));
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-2"));
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("tok-3"));
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-3"));
        let mut h = HeaderMap::new();
        h.insert("x-goog-api-key", HeaderValue::from_static("tok-4"));
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-4"));
        assert_eq!(extract_token(&HeaderMap::new(), None), None);
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Basic abc"));
        assert_eq!(extract_token(&h, None), None);
    }

    #[test]
    fn query_key_is_the_last_token_source() {
        assert_eq!(
            extract_token(&HeaderMap::new(), Some("key=query-tok")).as_deref(),
            Some("query-tok")
        );
        assert_eq!(
            extract_token(&HeaderMap::new(), Some("a=1&key=query-tok&b=2")).as_deref(),
            Some("query-tok")
        );
        assert!(extract_token(&HeaderMap::new(), Some("a=1")).is_none());
        assert!(extract_token(&HeaderMap::new(), Some("key=")).is_none());
        assert!(extract_token(&HeaderMap::new(), Some("keyless=1")).is_none());

        let mut h = HeaderMap::new();
        h.insert("x-goog-api-key", HeaderValue::from_static("header-tok"));
        assert_eq!(
            extract_token(&h, Some("key=query-tok")).as_deref(),
            Some("header-tok")
        );
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static("Bearer bearer-tok"),
        );
        h.insert("x-api-key", HeaderValue::from_static("api-tok"));
        assert_eq!(
            extract_token(&h, Some("key=query-tok")).as_deref(),
            Some("bearer-tok")
        );
    }

    #[test]
    fn query_rewrite_drops_only_the_key_parameter() {
        assert_eq!(strip_query_key("key=tok").as_deref(), None);
        assert_eq!(strip_query_key("").as_deref(), None);
        assert_eq!(
            strip_query_key("key=tok&alt=sse").as_deref(),
            Some("alt=sse")
        );
        assert_eq!(
            strip_query_key("alt=sse&key=tok").as_deref(),
            Some("alt=sse")
        );
        assert_eq!(
            strip_query_key("a=1&key=tok&b=2").as_deref(),
            Some("a=1&b=2")
        );
        assert_eq!(
            strip_query_key("key=one&key=two&a=1").as_deref(),
            Some("a=1")
        );
        assert_eq!(
            strip_query_key("apikey=1&keyless=2").as_deref(),
            Some("apikey=1&keyless=2")
        );
        assert_eq!(
            strip_query_key("q=a%20b&key=tok&r=%2F").as_deref(),
            Some("q=a%20b&r=%2F")
        );
    }

    #[test]
    fn a_proxy_credential_yields_its_token() {
        assert_eq!(proxy_token("Bearer abc").as_deref(), Some("abc"));
        // "user:tok" and ":tok" and "tok:" as basic credentials.
        assert_eq!(proxy_token("Basic dXNlcjp0b2s=").as_deref(), Some("tok"));
        assert_eq!(proxy_token("basic OnRvaw==").as_deref(), Some("tok"));
        assert_eq!(proxy_token("Basic dG9rOg==").as_deref(), Some("tok"));
        for bad in ["", "Bearer ", "Basic !!!", "Digest abc", "Basic"] {
            assert_eq!(proxy_token(bad), None, "{bad}");
        }
    }
}
