use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::classify::{parse_http_date, parse_retry_after};

/// Matches the Retry-After cap; a longer reset is not trusted as a wait.
const MAX_RESET: Duration = Duration::from_secs(24 * 60 * 60);
/// Epoch seconds reach this only in the year 5138, so a larger value must be milliseconds.
const EPOCH_MILLIS_FLOOR: u64 = 100_000_000_000;
/// Below this a value cannot be a current epoch in seconds.
const EPOCH_SECONDS_FLOOR: u64 = 1_000_000_000;

/// Limits one response reported. `None` means absent or unusable.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub request_limit: Option<u64>,
    pub requests_remaining: Option<u64>,
    pub requests_reset: Option<Duration>,
    pub token_limit: Option<u64>,
    pub tokens_remaining: Option<u64>,
    pub tokens_reset: Option<Duration>,
    pub retry_after: Option<Duration>,
    /// Response `Date` as unix seconds.
    pub date: Option<i64>,
}

pub fn observe(headers: &HeaderMap) -> Observed {
    observe_at(headers, local_now())
}

fn local_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

// Absolute resets use the provider's `Date`, so local clock skew does not shift them.
fn observe_at(headers: &HeaderMap, local_now: i64) -> Observed {
    let date = text(headers, "date")
        .and_then(parse_http_date)
        .filter(|secs| *secs >= 0);
    let clock = date.unwrap_or(local_now);
    let clock_time = UNIX_EPOCH
        .checked_add(Duration::from_secs(u64::try_from(clock).unwrap_or(0)))
        .unwrap_or(UNIX_EPOCH);

    // Family precedence per field: OpenAI-style, then Anthropic, then OpenRouter's
    // generic `x-ratelimit-*`, which only covers requests.
    Observed {
        request_limit: count(headers, "x-ratelimit-limit-requests")
            .or_else(|| count(headers, "anthropic-ratelimit-requests-limit"))
            .or_else(|| count(headers, "x-ratelimit-limit")),
        requests_remaining: count(headers, "x-ratelimit-remaining-requests")
            .or_else(|| count(headers, "anthropic-ratelimit-requests-remaining"))
            .or_else(|| count(headers, "x-ratelimit-remaining")),
        requests_reset: duration_reset(headers, "x-ratelimit-reset-requests")
            .or_else(|| rfc3339_reset(headers, "anthropic-ratelimit-requests-reset", clock))
            .or_else(|| epoch_reset(headers, "x-ratelimit-reset", clock)),
        token_limit: count(headers, "x-ratelimit-limit-tokens")
            .or_else(|| count(headers, "anthropic-ratelimit-tokens-limit")),
        tokens_remaining: count(headers, "x-ratelimit-remaining-tokens")
            .or_else(|| count(headers, "anthropic-ratelimit-tokens-remaining")),
        tokens_reset: duration_reset(headers, "x-ratelimit-reset-tokens")
            .or_else(|| rfc3339_reset(headers, "anthropic-ratelimit-tokens-reset", clock)),
        retry_after: text(headers, "retry-after")
            .and_then(|value| parse_retry_after(Some(value), clock_time)),
        date,
    }
}

fn text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn count(headers: &HeaderMap, name: &str) -> Option<u64> {
    text(headers, name)?.parse().ok()
}

fn duration_reset(headers: &HeaderMap, name: &str) -> Option<Duration> {
    text(headers, name)
        .and_then(parse_duration)
        .and_then(within_cap)
}

fn rfc3339_reset(headers: &HeaderMap, name: &str, clock: i64) -> Option<Duration> {
    let at = OffsetDateTime::parse(text(headers, name)?, &Rfc3339).ok()?;
    until(at.unix_timestamp_nanos() / 1_000_000, clock)
}

fn epoch_reset(headers: &HeaderMap, name: &str, clock: i64) -> Option<Duration> {
    let value: u64 = text(headers, name)?.parse().ok()?;
    // A small number is a delta in seconds, not an epoch: epoch seconds passed 1e9 in 2001.
    if value < EPOCH_SECONDS_FLOOR {
        return within_cap(Duration::from_secs(value));
    }
    let millis = if value >= EPOCH_MILLIS_FLOOR {
        value
    } else {
        value.checked_mul(1_000)?
    };
    until(i128::from(millis), clock)
}

fn until(at_millis: i128, clock: i64) -> Option<Duration> {
    let delta = (at_millis - i128::from(clock) * 1_000).max(0);
    within_cap(Duration::from_millis(u64::try_from(delta).ok()?))
}

fn within_cap(wait: Duration) -> Option<Duration> {
    (wait <= MAX_RESET).then_some(wait)
}

/// Integers only: plain seconds, or `h`/`m`/`s`/`ms` components such as `6m0s`.
fn parse_duration(value: &str) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().ok()?));
    }
    let mut total_millis: u64 = 0;
    let mut rest = value;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (number, tail) = rest.split_at(digits);
        let letters = tail.bytes().take_while(u8::is_ascii_alphabetic).count();
        let (unit, next) = tail.split_at(letters);
        let unit_millis: u64 = match unit {
            "h" => 3_600_000,
            "m" => 60_000,
            "s" => 1_000,
            "ms" => 1,
            _ => return None,
        };
        let amount: u64 = number.parse().ok()?;
        total_millis = total_millis.checked_add(amount.checked_mul(unit_millis)?)?;
        rest = next;
    }
    Some(Duration::from_millis(total_millis))
}

/// Starting limits; config fields override each one, and `None` sets no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetLimits {
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub concurrent: Option<u32>,
    pub daily_requests: Option<u32>,
    /// Floor for learning, so header evidence alone cannot stall the bucket.
    pub min_rpm: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    ZenFree,
    OpenrouterFree,
}

impl Preset {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "zen-free" => Ok(Self::ZenFree),
            "openrouter-free" => Ok(Self::OpenrouterFree),
            other => Err(format!("unknown shaper preset `{other}`")),
        }
    }

    pub const fn limits(self) -> PresetLimits {
        match self {
            // Documented free-tier caps; re-check on each release.
            Self::OpenrouterFree => PresetLimits {
                rpm: Some(20),
                tpm: None,
                concurrent: None,
                daily_requests: Some(50),
                min_rpm: Some(1),
            },
            // No published limits for Zen free; start conservative and let learning raise it.
            Self::ZenFree => PresetLimits {
                rpm: Some(10),
                tpm: None,
                concurrent: Some(2),
                daily_requests: None,
                min_rpm: Some(1),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_small_generic_reset_is_a_delta_in_seconds() {
        let h = headers(&[("x-ratelimit-reset", "30"), ("date", DATE)]);
        assert_eq!(
            observe_at(&h, 0).requests_reset,
            Some(Duration::from_secs(30))
        );
    }

    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    const DATE: &str = "Sat, 14 Nov 2026 12:00:00 GMT";
    const DATE_SECS: i64 = 1_794_657_600;
    const SKEWED_DATE: &str = "Sat, 14 Nov 2026 11:00:00 GMT";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn secs(n: u64) -> Option<Duration> {
        Some(Duration::from_secs(n))
    }

    #[test]
    fn openai_style_families_are_read() {
        let h = headers(&[
            ("x-ratelimit-limit-requests", "500"),
            ("x-ratelimit-remaining-requests", "499"),
            ("x-ratelimit-reset-requests", "6m0s"),
            ("x-ratelimit-limit-tokens", "200000"),
            ("x-ratelimit-remaining-tokens", "150000"),
            ("x-ratelimit-reset-tokens", "20ms"),
            ("date", DATE),
        ]);
        let o = observe(&h);
        assert_eq!(o.request_limit, Some(500));
        assert_eq!(o.requests_remaining, Some(499));
        assert_eq!(o.requests_reset, Some(Duration::from_secs(360)));
        assert_eq!(o.token_limit, Some(200_000));
        assert_eq!(o.tokens_remaining, Some(150_000));
        assert_eq!(o.tokens_reset, Some(Duration::from_millis(20)));
        assert_eq!(o.date, Some(DATE_SECS));
    }

    #[test]
    fn plain_seconds_and_seconds_units_are_read() {
        let h = headers(&[
            ("x-ratelimit-reset-requests", "1s"),
            ("x-ratelimit-reset-tokens", "2"),
        ]);
        let o = observe(&h);
        assert_eq!(o.requests_reset, secs(1));
        assert_eq!(o.tokens_reset, secs(2));
    }

    #[test]
    fn openrouter_reset_in_epoch_millis_and_seconds() {
        let millis = headers(&[
            ("x-ratelimit-limit", "20"),
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1794657660000"),
            ("date", DATE),
        ]);
        let o = observe(&millis);
        assert_eq!(o.request_limit, Some(20));
        assert_eq!(o.requests_remaining, Some(0));
        assert_eq!(o.requests_reset, secs(60));

        let seconds = headers(&[("x-ratelimit-reset", "1794657660"), ("date", DATE)]);
        assert_eq!(observe(&seconds).requests_reset, secs(60));
    }

    #[test]
    fn anthropic_rfc3339_resets_are_read() {
        let h = headers(&[
            ("anthropic-ratelimit-requests-limit", "50"),
            ("anthropic-ratelimit-requests-remaining", "3"),
            ("anthropic-ratelimit-requests-reset", "2026-11-14T12:01:30Z"),
            ("anthropic-ratelimit-tokens-limit", "40000"),
            ("anthropic-ratelimit-tokens-remaining", "0"),
            ("anthropic-ratelimit-tokens-reset", "2026-11-14T12:00:45Z"),
            ("date", DATE),
        ]);
        let o = observe(&h);
        assert_eq!(o.request_limit, Some(50));
        assert_eq!(o.requests_remaining, Some(3));
        assert_eq!(o.requests_reset, secs(90));
        assert_eq!(o.token_limit, Some(40_000));
        assert_eq!(o.tokens_remaining, Some(0));
        assert_eq!(o.tokens_reset, secs(45));
    }

    #[test]
    fn retry_after_seconds_and_http_date_are_read() {
        assert_eq!(
            observe(&headers(&[("retry-after", "30")])).retry_after,
            secs(30)
        );
        let h = headers(&[
            ("retry-after", "Sat, 14 Nov 2026 12:02:00 GMT"),
            ("date", DATE),
        ]);
        assert_eq!(observe(&h).retry_after, secs(120));
    }

    #[test]
    fn absolute_resets_use_the_response_date_not_the_local_clock() {
        // The provider clock runs an hour behind, so the 12:01:30 reset is 3690 s away.
        let h = headers(&[
            ("anthropic-ratelimit-requests-reset", "2026-11-14T12:01:30Z"),
            ("retry-after", "Sat, 14 Nov 2026 12:02:00 GMT"),
            ("date", SKEWED_DATE),
        ]);
        let o = observe_at(&h, 1);
        assert_eq!(o.requests_reset, secs(3690));
        assert_eq!(o.retry_after, secs(3720));
        assert_eq!(o.date, Some(DATE_SECS - 3600));
        // The local clock must not matter once Date is present.
        assert_eq!(observe_at(&h, 1), observe_at(&h, 9_999_999_999));
    }

    #[test]
    fn absolute_reset_falls_back_to_the_local_clock_without_date() {
        let h = headers(&[("anthropic-ratelimit-requests-reset", "2026-11-14T12:01:30Z")]);
        let o = observe_at(&h, DATE_SECS - 30);
        assert_eq!(o.requests_reset, secs(120));
        assert_eq!(o.date, None);
    }

    #[test]
    fn resets_more_than_24_hours_away_are_ignored() {
        let at_cap = headers(&[
            ("anthropic-ratelimit-requests-reset", "2026-11-15T12:00:00Z"),
            ("date", DATE),
        ]);
        assert_eq!(observe(&at_cap).requests_reset, secs(86_400));

        let over = headers(&[
            ("anthropic-ratelimit-requests-reset", "2026-11-15T12:00:01Z"),
            ("x-ratelimit-reset-tokens", "25h0m0s"),
            ("date", DATE),
        ]);
        let o = observe(&over);
        assert_eq!(o.requests_reset, None);
        assert_eq!(o.tokens_reset, None);
    }

    #[test]
    fn garbage_values_are_ignored() {
        let mut h = headers(&[
            ("x-ratelimit-limit-requests", "-1"),
            ("x-ratelimit-remaining-requests", "abc"),
            ("x-ratelimit-reset-requests", "1.5s"),
            ("x-ratelimit-limit-tokens", "99999999999999999999999999"),
            ("x-ratelimit-reset-tokens", "99999999999999999h"),
            ("anthropic-ratelimit-tokens-reset", "2026-13-99T00:00:00Z"),
            ("x-ratelimit-reset", "-5"),
            ("retry-after", "soon"),
            ("date", "not a date"),
        ]);
        h.insert(
            HeaderName::from_static("anthropic-ratelimit-tokens-remaining"),
            HeaderValue::from_bytes(&[0xff, 0x31]).unwrap(),
        );
        assert_eq!(observe(&h), Observed::default());
    }

    #[test]
    fn empty_duration_is_ignored() {
        let h = headers(&[("x-ratelimit-reset-requests", "")]);
        assert_eq!(observe(&h).requests_reset, None);
    }

    #[test]
    fn mixed_families_in_one_response() {
        let h = headers(&[
            ("x-ratelimit-limit-requests", "500"),
            ("x-ratelimit-limit", "20"),
            ("anthropic-ratelimit-tokens-remaining", "1000"),
            ("anthropic-ratelimit-requests-remaining", "7"),
            ("retry-after", "5"),
            ("date", DATE),
        ]);
        let o = observe(&h);
        // The OpenAI-style value wins over OpenRouter's generic one.
        assert_eq!(o.request_limit, Some(500));
        assert_eq!(o.requests_remaining, Some(7));
        assert_eq!(o.tokens_remaining, Some(1_000));
        assert_eq!(o.retry_after, secs(5));
        assert_eq!(o.token_limit, None);
    }

    #[test]
    fn preset_names_parse() {
        assert_eq!(Preset::parse("zen-free"), Ok(Preset::ZenFree));
        assert_eq!(Preset::parse("openrouter-free"), Ok(Preset::OpenrouterFree));
    }

    #[test]
    fn unknown_preset_is_an_error() {
        assert!(Preset::parse("zen").is_err());
        assert!(Preset::parse("").is_err());
    }

    #[test]
    fn preset_limits_match_the_documented_values() {
        let openrouter = Preset::OpenrouterFree.limits();
        assert_eq!(openrouter.rpm, Some(20));
        assert_eq!(openrouter.daily_requests, Some(50));
        assert_eq!(openrouter.tpm, None);

        let zen = Preset::ZenFree.limits();
        assert_eq!(zen.rpm, Some(10));
        assert_eq!(zen.concurrent, Some(2));
        assert_eq!(zen.daily_requests, None);
    }
}
