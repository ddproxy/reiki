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

/// Fixed text for a token under [`MIN_TOKEN_LEN`]: the public minimum, never the supplied length.
const TOKEN_TOO_SHORT: &str = "token must be at least 16 characters";

/// The key a failure is counted under: an IPv4 address as is, an IPv6 address by its /64 so a
/// host that rotates through its prefix shares one count. A mapped IPv4 address (a dual-stack
/// listener's IPv4 peer) is counted as the IPv4 address it carries.
fn throttle_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut octets = v6.octets();
            octets[8..].fill(0);
            IpAddr::V6(octets.into())
        }
    }
}

/// One attempt reserved by [`LoginThrottle::reserve`]. Dropping it on a failed login leaves the
/// attempt counted; a listener that does not count successes can give it back with
/// [`LoginThrottle::release`] instead.
#[derive(Debug, Clone, Copy)]
pub struct LoginAttempt {
    key: IpAddr,
    /// Start of the window the reservation belongs to, so a release cannot undo a later window.
    since: Instant,
    /// Number of counted failures after this reservation.
    failures: u32,
    counted: bool,
}

impl LoginAttempt {
    /// How many failures are counted for the address after this reservation.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

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

    /// Frees a slot for a new address when the table is full. Callers hold the `seen` lock.
    fn make_room(
        seen: &mut HashMap<IpAddr, (u32, Instant)>,
        key: IpAddr,
        now: Instant,
        settings: ThrottleSettings,
    ) {
        if seen.len() < MAX_TRACKED || seen.contains_key(&key) {
            return;
        }
        // Free one slot: an expired entry, else the least-failed oldest unblocked one, else
        // the oldest blocked one, so a flood of blocked addresses cannot leave new ones untracked.
        let victim = seen.iter().min_by_key(|(_, (count, since))| {
            let expired = now.saturating_duration_since(*since) >= settings.window;
            (!expired, *count >= settings.max_failures, *count, *since)
        });
        if let Some((victim, _)) = victim {
            let victim = *victim;
            seen.remove(&victim);
        }
    }

    /// Checks whether `ip` may try and reserves the attempt in one lock, so a concurrent burst
    /// cannot all pass the check before any attempt is counted. `Ok` carries the reservation;
    /// `Err` is how long the address must wait.
    pub fn reserve(&self, ip: IpAddr, now: Instant) -> Result<LoginAttempt, Duration> {
        let key = throttle_key(ip);
        let settings = self.settings();
        if !settings.enabled {
            return Ok(LoginAttempt {
                key,
                since: now,
                failures: 0,
                counted: false,
            });
        }
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        Self::make_room(&mut seen, key, now, settings);
        let entry = seen.entry(key).or_insert((0, now));
        if now.saturating_duration_since(entry.1) >= settings.window {
            *entry = (0, now);
        }
        if entry.0 >= settings.max_failures {
            return Err(settings.window - now.saturating_duration_since(entry.1));
        }
        entry.0 += 1;
        Ok(LoginAttempt {
            key,
            since: entry.1,
            failures: entry.0,
            counted: true,
        })
    }

    /// Gives back a reserved attempt without clearing earlier failures, for listeners that do
    /// not reset the count on a successful login. A reservation from another window is ignored.
    pub fn release(&self, attempt: LoginAttempt) {
        if !attempt.counted {
            return;
        }
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = seen.get_mut(&attempt.key)
            && entry.1 == attempt.since
            && entry.0 > 0
        {
            entry.0 -= 1;
        }
    }

    /// How long `ip` must wait, or `None` when it may try.
    pub fn blocked(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let ip = throttle_key(ip);
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
        let ip = throttle_key(ip);
        let settings = self.settings();
        if !settings.enabled {
            return 0;
        }
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        Self::make_room(&mut seen, ip, now, settings);
        let entry = seen.entry(ip).or_insert((0, now));
        if now.saturating_duration_since(entry.1) >= settings.window {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.0
    }

    /// A good login clears the address's count.
    pub fn succeed(&self, ip: IpAddr) {
        let ip = throttle_key(ip);
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.remove(&ip);
    }
}

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hashes a token for `token_sha256`, refusing one shorter than [`MIN_TOKEN_LEN`].
///
/// The error is a fixed description rather than one derived from the token: callers print it on
/// stderr, and a length or fragment of the token must never reach the output.
pub fn hash_token(token: &str) -> Result<String, &'static str> {
    let token = token.trim();
    if token.chars().count() < MIN_TOKEN_LEN {
        return Err(TOKEN_TOO_SHORT);
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

/// The credential of an `Authorization: Bearer <token>` value. The scheme name is
/// case-insensitive, and the same surrounding whitespace is accepted as on the client listener.
pub(crate) fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, rest) = value
        .trim_ascii()
        .split_once(|c: char| c.is_ascii_whitespace())?;
    let token = rest.trim_ascii();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// The token in a `Proxy-Authorization` value: a bearer token, or the password of a basic
/// credential (the user name when the password is empty), as `HTTPS_PROXY=http://user:token@host` sends it.
pub fn proxy_token(value: &str) -> Option<String> {
    if let Some(token) = bearer_token(value) {
        return Some(token.to_owned());
    }
    let (scheme, rest) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(base64_decode(rest.trim())?).ok()?;
    let (user, pass) = decoded.split_once(':').unwrap_or((decoded.as_str(), ""));
    let token = if pass.is_empty() { user } else { pass };
    (!token.is_empty()).then(|| token.to_owned())
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
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);
    if let Some(token) = bearer {
        return Some(token.to_owned());
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
    #[test]
    fn the_short_token_message_states_the_minimum_without_the_supplied_length() {
        let err = hash_token("short").unwrap_err();
        assert!(err.contains(&MIN_TOKEN_LEN.to_string()), "{err}");
        assert!(!err.contains('5'), "the supplied length must not appear");
        assert!(hash_token(&"x".repeat(MIN_TOKEN_LEN)).is_ok());
    }

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
        let attempt = throttle.reserve(ip, t0).unwrap();
        assert_eq!(attempt.failures(), 0, "a disabled backoff counts nothing");
        throttle.release(attempt);
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
    fn a_full_table_of_blocked_addresses_evicts_the_oldest_to_track_a_newcomer() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        for n in 0..MAX_TRACKED as u32 {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(n + 1));
            for _ in 0..MAX_FAILURES {
                throttle.fail(ip, t0 + Duration::from_millis(u64::from(n)));
            }
        }
        let now = t0 + Duration::from_secs(1);
        let newcomer: IpAddr = "203.0.113.7".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            throttle.fail(newcomer, now);
        }
        assert_eq!(throttle.seen.lock().unwrap().len(), MAX_TRACKED);
        assert!(
            throttle.blocked(newcomer, now).is_some(),
            "tracked and blocked"
        );
        let oldest = IpAddr::from(std::net::Ipv4Addr::from(1u32));
        assert!(
            throttle.blocked(oldest, now).is_none(),
            "the oldest was evicted"
        );
        let next = IpAddr::from(std::net::Ipv4Addr::from(2u32));
        assert!(throttle.blocked(next, now).is_some());
    }

    #[test]
    fn a_full_table_keeps_the_blocked_and_evicts_a_failing_address() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        let blocked: IpAddr = "192.0.2.1".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            throttle.fail(blocked, t0);
        }
        for n in 1..MAX_TRACKED as u32 {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000 + n));
            throttle.fail(ip, t0 + Duration::from_millis(u64::from(n)));
        }
        let now = t0 + Duration::from_secs(1);
        let newcomer: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(throttle.fail(newcomer, now), 1);
        assert!(throttle.blocked(blocked, now).is_some());
        assert_eq!(throttle.seen.lock().unwrap().len(), MAX_TRACKED);
    }

    #[test]
    fn mapped_ipv4_peers_are_counted_apart() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        let bad: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        let other: IpAddr = "::ffff:203.0.113.9".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            throttle.fail(bad, t0);
        }
        assert!(throttle.blocked(bad, t0).is_some());
        assert!(throttle.blocked(other, t0).is_none());
        let plain: IpAddr = "203.0.113.5".parse().unwrap();
        assert!(
            throttle.blocked(plain, t0).is_some(),
            "mapped and plain share a count"
        );
    }

    #[test]
    fn an_ipv6_prefix_shares_one_count() {
        let throttle = LoginThrottle::default();
        let t0 = Instant::now();
        for n in 0..MAX_FAILURES {
            let ip: IpAddr = format!("2001:db8:1:2::{:x}", n + 1).parse().unwrap();
            throttle.fail(ip, t0);
        }
        let sibling: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        assert!(
            throttle.blocked(sibling, t0).is_some(),
            "same /64, same block"
        );
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(
            throttle.blocked(other, t0).is_none(),
            "another /64 is unaffected"
        );
        throttle.succeed(sibling);
        assert!(throttle.blocked(sibling, t0).is_none());
    }

    #[test]
    fn a_concurrent_burst_cannot_pass_the_check_before_failures_are_recorded() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        const N: usize = 32;
        let throttle = Arc::new(LoginThrottle::new(ThrottleSettings {
            enabled: true,
            max_failures: 1,
            window: Duration::from_secs(60),
        }));
        let ip: IpAddr = "192.0.2.44".parse().unwrap();
        let now = Instant::now();
        let start = Arc::new(Barrier::new(N));
        let checked = Arc::new(Barrier::new(N));
        let allowed = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..N)
            .map(|_| {
                let throttle = Arc::clone(&throttle);
                let start = Arc::clone(&start);
                let checked = Arc::clone(&checked);
                let allowed = Arc::clone(&allowed);
                std::thread::spawn(move || {
                    start.wait();
                    if throttle.reserve(ip, now).is_ok() {
                        allowed.fetch_add(1, Ordering::SeqCst);
                    }
                    checked.wait();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            allowed.load(Ordering::SeqCst),
            1,
            "only the first attempt may pass the check"
        );
        assert!(throttle.blocked(ip, now).is_some(), "it is counted at once");
    }

    #[test]
    fn releasing_a_reservation_keeps_earlier_failures() {
        let throttle = LoginThrottle::new(ThrottleSettings {
            enabled: true,
            max_failures: 3,
            window: Duration::from_secs(60),
        });
        let ip: IpAddr = "192.0.2.45".parse().unwrap();
        let now = Instant::now();
        let first = throttle.reserve(ip, now).unwrap();
        let second = throttle.reserve(ip, now).unwrap();
        assert_eq!(second.failures(), 2);
        throttle.release(first);
        assert_eq!(
            throttle.fail(ip, now),
            2,
            "only the given-up attempt was removed"
        );
        throttle.fail(ip, now);
        assert!(throttle.blocked(ip, now).is_some());
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
        assert!(!err.contains("shrt"), "the error must not echo the token");
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
    fn bearer_scheme_is_case_insensitive_but_the_token_is_exact() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer Abc"), Some("Abc"));
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearerabc"), None);
        assert_eq!(bearer_token("  Bearer   abc  "), Some("abc"));
        assert_eq!(bearer_token("Bearer\tabc"), Some("abc"));
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearer   "), None);
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
