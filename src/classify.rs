use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_CLASSIFY_BODY: usize = 64 * 1024;

// One place for the built-in phrases, so the failover table and the classifier cannot drift.
const BUILTIN_REQUEST_ERROR_PHRASES: &[&str] = &["fewer max_tokens", "can only afford"];
const BUILTIN_QUOTA_400_PHRASES: &[&str] = &["credit balance is too low"];
const BUILTIN_QUOTA_429_PHRASES: &[&str] = &[
    "usage exceeded",
    "quota",
    "subscribe to go",
    "free usage limit",
    "free-models-per-day",
    "insufficient",
];
const BUILTIN_RATE_LIMIT_PHRASES: &[&str] = &["rate limit"];
const BUILTIN_INVALID_CREDENTIAL_PHRASES: &[&str] = &[
    "invalid api key",
    "incorrect api key",
    "api key is invalid",
    "api key not valid",
    "api key is not valid",
    "api key has expired",
    "api key has been revoked",
    "api key is disabled",
    "api_key_invalid",
    "invalid_api_key",
    "invalid x-api-key",
    "invalid credential",
    "credential has been revoked",
    "credential has expired",
    "invalid token",
    "token has been revoked",
    "token has expired",
];
const BUILTIN_ACCESS_DISABLED_PHRASES: &[&str] =
    &["access is disabled", "free tier", "not enabled"];
const BUILTIN_ACCESS_DISABLED_5XX_PHRASES: &[&str] =
    &["model access is disabled", "access is disabled"];
const BUILTIN_EDGE_BLOCK_PHRASES: &[&str] = &["error code: 10"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    RateLimited(Option<Duration>),
    QuotaExhausted,
    AccessDisabled,
    Unauthorized,
    Transient,
}

impl From<FailureClass> for reiki_plugin::FailureKind {
    fn from(class: FailureClass) -> Self {
        match class {
            FailureClass::RateLimited(_) => Self::RateLimited,
            FailureClass::QuotaExhausted => Self::QuotaExhausted,
            FailureClass::AccessDisabled => Self::AccessDisabled,
            FailureClass::Unauthorized => Self::Unauthorized,
            FailureClass::Transient => Self::Transient,
        }
    }
}

impl FailureClass {
    /// The same name as the `class` label of `reiki_cooldowns_total`, so operators see one
    /// vocabulary across `/status`, the peer view and metrics.
    pub fn name(self) -> &'static str {
        reiki_plugin::FailureKind::from(self).label()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    KeyFailure(FailureClass),
    /// The request itself is too large for this account; another key may afford it.
    RequestError,
    PassThrough,
}

pub fn parse_retry_after(value: Option<&str>, now: SystemTime) -> Option<Duration> {
    let value = value?.trim();
    match value.parse::<u64>() {
        Ok(seconds) => Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER)),
        Err(_) => http_date_delta(value, now),
    }
}

fn http_date_delta(value: &str, now: SystemTime) -> Option<Duration> {
    let then = parse_http_date(value)?;
    let now_secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // A date in the past means the window has already closed, so the key is usable now.
    let delta = then.saturating_sub(now_secs as i64).max(0) as u64;
    Some(Duration::from_secs(delta).min(MAX_RETRY_AFTER))
}

// IMF-fixdate, the only form RFC 9110 requires: `Sun, 06 Nov 1994 08:49:37 GMT`.
pub(crate) fn parse_http_date(value: &str) -> Option<i64> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = tokens.as_slice() else {
        return None;
    };
    if day.len() != 2 || year.len() != 4 {
        return None;
    }
    let day = parse_number(day)?;
    let month = month_number(month)?;
    let year = parse_number(year)?;
    let (hour, minute, second) = parse_time(time)?;
    build_epoch(year, month, day, hour, minute, second)
}

fn parse_number(value: &str) -> Option<i64> {
    value.parse::<i64>().ok()
}

fn parse_time(value: &str) -> Option<(i64, i64, i64)> {
    let mut parts = value.split(':');
    let hour = parse_number(parts.next()?)?;
    let minute = parse_number(parts.next()?)?;
    let second = parse_number(parts.next()?)?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second))
}

fn month_number(value: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(value))
        .map(|i| i as i64 + 1)
}

fn build_epoch(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
) -> Option<i64> {
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// Days since 1970-01-01, Proleptic Gregorian (Howard Hinnant's days_from_civil).
pub(crate) fn days_from_civil(mut year: i64, month: i64, day: i64) -> i64 {
    if month <= 2 {
        year -= 1;
    }
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// True when `"field": "value"` appears as real JSON. Text a client sent that an upstream
/// echoes inside a string has its quotes escaped, so it cannot imitate a field.
fn has_json_pair(body: &str, field: &str, value: &str) -> bool {
    let key = format!("\"{field}\"");
    let mut from = 0;
    while let Some(at) = body[from..].find(&key) {
        let start = from + at;
        from = start + key.len();
        if body[..start].ends_with('\\') {
            continue;
        }
        let Some(rest) = body[from..].trim_start().strip_prefix(':') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if rest.strip_prefix(value).is_some_and(|r| r.starts_with('"')) {
            return true;
        }
    }
    false
}

fn is_edge_block_page(body: &str) -> bool {
    contains_any(body, BUILTIN_EDGE_BLOCK_PHRASES)
}

/// 403 bodies that establish a bad credential rather than a request-scoped refusal.
fn names_invalid_credential(body: &str) -> bool {
    contains_any(body, BUILTIN_INVALID_CREDENTIAL_PHRASES)
}

pub fn classify(status: u16, retry_after: Option<&str>, body_snippet: &str) -> Outcome {
    classify_at(status, retry_after, body_snippet, SystemTime::now())
}

/// Extra body phrases configured on an upstream. They extend the built-in lists.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtraPhrases<'a> {
    pub request_error: &'a [String],
    pub quota: &'a [String],
    pub rate_limit: &'a [String],
}

/// The class and whether a configured quota phrase chose it, which decides the cooldown scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub outcome: Outcome,
    pub quota_phrase_matched: bool,
}

fn is_request_size_error(body: &str) -> bool {
    let matches = |message: &str| contains_any(message, BUILTIN_REQUEST_ERROR_PHRASES);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return matches(body);
    };
    let error_message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str);
    let message = value.get("message").and_then(serde_json::Value::as_str);
    error_message.is_some_and(matches) || message.is_some_and(matches)
}

pub fn classify_at(
    status: u16,
    retry_after: Option<&str>,
    body_snippet: &str,
    now: SystemTime,
) -> Outcome {
    classify_with(
        status,
        retry_after,
        body_snippet,
        now,
        &ExtraPhrases::default(),
    )
    .outcome
}

/// Classifies an upstream error with the configured extra phrases as well as the built-in ones.
/// Only the first 64 KiB of the body is read; a multi-byte character at the cut is dropped.
pub fn classify_with(
    status: u16,
    retry_after: Option<&str>,
    body_snippet: &str,
    now: SystemTime,
    extra: &ExtraPhrases<'_>,
) -> Classification {
    let end = body_snippet.len().min(MAX_CLASSIFY_BODY);
    let body = String::from_utf8_lossy(&body_snippet.as_bytes()[..end]).to_lowercase();
    let quota_phrase = contains_extra(&body, extra.quota);
    let rate_limit_phrase = contains_extra(&body, extra.rate_limit);
    let request_error = matches!(status, 400 | 402 | 413)
        && (is_request_size_error(&body) || contains_extra(&body, extra.request_error));

    // A request-size error is the client's to fix: the key stays healthy and the proxy tries the
    // next key, since another account may afford the request.
    if request_error {
        return Classification {
            outcome: Outcome::RequestError,
            quota_phrase_matched: false,
        };
    }

    let outcome = match status {
        200..=399 => Outcome::Success,
        401 => Outcome::KeyFailure(FailureClass::Unauthorized),
        400 => {
            if has_json_pair(&body, "reason", "api_key_invalid") {
                Outcome::KeyFailure(FailureClass::Unauthorized)
            } else if quota_phrase || contains_any(&body, BUILTIN_QUOTA_400_PHRASES) {
                Outcome::KeyFailure(FailureClass::QuotaExhausted)
            } else if rate_limit_phrase {
                Outcome::KeyFailure(FailureClass::RateLimited(parse_retry_after(
                    retry_after,
                    now,
                )))
            } else {
                Outcome::PassThrough
            }
        }
        402 | 413 => {
            if quota_phrase {
                Outcome::KeyFailure(FailureClass::QuotaExhausted)
            } else if rate_limit_phrase {
                Outcome::KeyFailure(FailureClass::RateLimited(parse_retry_after(
                    retry_after,
                    now,
                )))
            } else if status == 402 {
                Outcome::KeyFailure(FailureClass::QuotaExhausted)
            } else {
                Outcome::PassThrough
            }
        }
        429 => {
            if quota_phrase || contains_any(&body, BUILTIN_QUOTA_429_PHRASES) {
                Outcome::KeyFailure(FailureClass::QuotaExhausted)
            } else {
                Outcome::KeyFailure(FailureClass::RateLimited(parse_retry_after(
                    retry_after,
                    now,
                )))
            }
        }
        403 => {
            if is_edge_block_page(&body) {
                Outcome::PassThrough
            } else if contains_any(&body, BUILTIN_ACCESS_DISABLED_PHRASES) {
                Outcome::KeyFailure(FailureClass::AccessDisabled)
            } else if names_invalid_credential(&body) {
                Outcome::KeyFailure(FailureClass::Unauthorized)
            } else {
                // A bare 403 is usually a WAF, geo-block or plan/model restriction that may only
                // apply to this request: the key is skipped for it, not cooled.
                Outcome::KeyFailure(FailureClass::AccessDisabled)
            }
        }
        500..=599 => {
            if quota_phrase {
                Outcome::KeyFailure(FailureClass::QuotaExhausted)
            } else if contains_any(&body, BUILTIN_ACCESS_DISABLED_5XX_PHRASES) {
                Outcome::KeyFailure(FailureClass::AccessDisabled)
            } else if rate_limit_phrase || contains_any(&body, BUILTIN_RATE_LIMIT_PHRASES) {
                Outcome::KeyFailure(FailureClass::RateLimited(parse_retry_after(
                    retry_after,
                    now,
                )))
            } else {
                Outcome::KeyFailure(FailureClass::Transient)
            }
        }
        _ => Outcome::PassThrough,
    };
    Classification {
        quota_phrase_matched: quota_phrase
            && matches!(outcome, Outcome::KeyFailure(FailureClass::QuotaExhausted)),
        outcome,
    }
}

fn contains_extra(body: &str, phrases: &[String]) -> bool {
    phrases.iter().any(|phrase| {
        let phrase = phrase.to_lowercase();
        body.contains(phrase.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_names_match_the_cooldown_metric_labels() {
        assert_eq!(FailureClass::RateLimited(None).name(), "rate_limited");
        assert_eq!(FailureClass::QuotaExhausted.name(), "quota_exhausted");
        assert_eq!(FailureClass::AccessDisabled.name(), "access_disabled");
        assert_eq!(FailureClass::Unauthorized.name(), "unauthorized");
        assert_eq!(FailureClass::Transient.name(), "transient");
    }

    #[test]
    fn success_and_redirects_are_delivered() {
        assert_eq!(classify(200, None, ""), Outcome::Success);
        assert_eq!(classify(204, None, ""), Outcome::Success);
        assert_eq!(classify(302, None, ""), Outcome::Success);
    }

    #[test]
    fn retry_after_delta_seconds_is_used() {
        assert_eq!(
            parse_retry_after(Some("30"), UNIX_EPOCH),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn retry_after_accepts_a_future_http_date() {
        assert_eq!(
            parse_retry_after(Some("Thu, 01 Jan 1970 00:10:00 GMT"), UNIX_EPOCH),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn a_past_http_date_yields_zero() {
        assert_eq!(
            parse_retry_after(Some("Wed, 31 Dec 1969 23:59:00 GMT"), UNIX_EPOCH),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn an_http_date_over_the_cap_is_clamped() {
        assert_eq!(
            parse_retry_after(Some("Fri, 02 Jan 1970 01:00:00 GMT"), UNIX_EPOCH),
            Some(MAX_RETRY_AFTER)
        );
    }

    #[test]
    fn a_malformed_retry_after_is_none() {
        assert_eq!(parse_retry_after(Some("not a date"), UNIX_EPOCH), None);
    }

    #[test]
    fn a_leap_day_is_valid_and_an_impossible_day_is_not() {
        assert_eq!(
            parse_retry_after(Some("Thu, 29 Feb 2024 00:00:00 GMT"), UNIX_EPOCH),
            Some(MAX_RETRY_AFTER)
        );
        assert_eq!(
            parse_retry_after(Some("Fri, 30 Feb 2024 00:00:00 GMT"), UNIX_EPOCH),
            None
        );
    }

    #[test]
    fn rate_limit_uses_retry_after_seconds() {
        assert_eq!(
            classify(429, Some("30"), "Rate limit exceeded"),
            Outcome::KeyFailure(FailureClass::RateLimited(Some(Duration::from_secs(30))))
        );
        assert_eq!(
            classify(429, Some("not a date"), "slow down"),
            Outcome::KeyFailure(FailureClass::RateLimited(None))
        );
        assert_eq!(
            classify(429, None, ""),
            Outcome::KeyFailure(FailureClass::RateLimited(None))
        );
    }

    #[test]
    fn a_free_usage_limit_is_quota_not_rate_limit() {
        let body =
            r#"{"error":{"type":"FreeUsageLimitError","message":"Free usage limit reached."}}"#;
        assert_eq!(
            classify(429, Some("60"), body),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn exhausted_allowance_is_quota_not_rate_limit() {
        for body in [
            "Free usage exceeded, subscribe to Go",
            "You exceeded your current quota",
            "insufficient balance",
        ] {
            assert_eq!(
                classify(429, Some("5"), body),
                Outcome::KeyFailure(FailureClass::QuotaExhausted),
                "{body}"
            );
        }
        assert_eq!(
            classify(402, None, ""),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn openrouter_402_request_too_large_is_a_request_error() {
        let body = r#"{"error":{"message":"This request requires more credits, or fewer max_tokens. You requested up to 256000 tokens, but can only afford 199460. ..."}}"#;
        assert_eq!(classify(402, None, body), Outcome::RequestError);
    }

    #[test]
    fn openrouter_402_other_messages_still_cool() {
        assert_eq!(
            classify(402, None, "insufficient credits"),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn openrouter_402_matching_is_case_insensitive() {
        let body = "You requested more, please use FEWER MAX_TOKENS.";
        assert_eq!(classify(402, None, body), Outcome::RequestError);
        let body2 = "CAN ONLY AFFORD 100 tokens";
        assert_eq!(classify(402, None, body2), Outcome::RequestError);
    }

    #[test]
    fn a_json_402_only_matches_the_message_fields() {
        let in_message = r#"{"error":{"message":"please use fewer max_tokens"}}"#;
        assert_eq!(classify(402, None, in_message), Outcome::RequestError);
        let top_level = r#"{"message":"can only afford 10 tokens"}"#;
        assert_eq!(classify(402, None, top_level), Outcome::RequestError);
        let elsewhere = r#"{"error":{"type":"fewer max_tokens"},"message":"insufficient credits"}"#;
        assert_eq!(
            classify(402, None, elsewhere),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn a_json_402_without_the_phrase_does_not_fall_back_to_the_whole_body() {
        let body = r#"{"error":{"code":"can only afford"},"detail":"insufficient credits"}"#;
        assert_eq!(
            classify(402, None, body),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn a_daily_free_model_429_is_a_quota_not_a_rate_limit() {
        let body = r#"{"error":{"message":"Rate limit exceeded: free-models-per-day. Add 10 credits to unlock 1000 free model requests per day"}}"#;
        assert_eq!(
            classify(429, None, body),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn disabled_model_access_is_its_own_class() {
        assert_eq!(
            classify(
                403,
                None,
                "Upstream request failed: Model access is disabled"
            ),
            Outcome::KeyFailure(FailureClass::AccessDisabled)
        );
        assert_eq!(
            classify(
                500,
                None,
                "Upstream request failed: Model access is disabled"
            ),
            Outcome::KeyFailure(FailureClass::AccessDisabled)
        );
        assert_eq!(
            classify(
                403,
                None,
                "OpenCode's free tier can only be used from within OpenCode"
            ),
            Outcome::KeyFailure(FailureClass::AccessDisabled)
        );
    }

    #[test]
    fn auth_failures_disable_the_key() {
        assert_eq!(
            classify(401, None, "invalid api key"),
            Outcome::KeyFailure(FailureClass::Unauthorized)
        );
        for body in [
            "invalid api key",
            r#"{"error":{"message":"API key is not valid"}}"#,
            r#"{"error":{"type":"invalid_api_key"}}"#,
            "the credential has been revoked",
            "token has expired",
        ] {
            assert_eq!(
                classify(403, None, body),
                Outcome::KeyFailure(FailureClass::Unauthorized),
                "{body}"
            );
        }
    }

    #[test]
    fn a_generic_403_is_a_request_scoped_refusal() {
        for body in [
            "forbidden",
            r#"{"error":{"message":"request blocked by security policy"}}"#,
            r#"{"error":{"message":"this model is not available on your plan"}}"#,
        ] {
            assert_eq!(
                classify(403, None, body),
                Outcome::KeyFailure(FailureClass::AccessDisabled),
                "{body}"
            );
        }
    }

    #[test]
    fn server_errors_are_transient() {
        assert_eq!(
            classify(502, None, ""),
            Outcome::KeyFailure(FailureClass::Transient)
        );
        assert_eq!(
            classify(503, None, "overloaded"),
            Outcome::KeyFailure(FailureClass::Transient)
        );
    }

    #[test]
    fn bad_requests_are_not_the_keys_fault() {
        assert_eq!(classify(400, None, "bad json"), Outcome::PassThrough);
        assert_eq!(classify(404, None, ""), Outcome::PassThrough);
        assert_eq!(classify(422, None, ""), Outcome::PassThrough);
    }

    #[test]
    fn an_edge_block_page_is_not_the_keys_fault() {
        assert_eq!(
            classify(403, None, "error code: 1010\n"),
            Outcome::PassThrough
        );
    }

    #[test]
    fn body_matching_ignores_case() {
        assert_eq!(
            classify(429, None, "USAGE EXCEEDED"),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn a_rejected_api_key_reported_as_a_400_is_unauthorized() {
        let body = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#;
        assert_eq!(
            classify(400, None, body),
            Outcome::KeyFailure(FailureClass::Unauthorized)
        );
    }

    #[test]
    fn an_empty_credit_balance_reported_as_a_400_is_quota_exhausted() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#;
        assert_eq!(
            classify(400, None, body),
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn other_400_responses_pass_through() {
        assert_eq!(
            classify(
                400,
                None,
                r#"{"error":{"message":"max_tokens is required"}}"#
            ),
            Outcome::PassThrough
        );
    }

    #[test]
    fn an_echoed_key_invalid_marker_does_not_disable_a_key() {
        let echoed =
            r#"{"error":{"message":"Unrecognized request argument supplied: api_key_invalid"}}"#;
        assert_eq!(classify(400, None, echoed), Outcome::PassThrough);
        let escaped = r#"{"error":{"message":"bad field \"reason\": \"API_KEY_INVALID\""}}"#;
        assert_eq!(classify(400, None, escaped), Outcome::PassThrough);
        let real = r#"{"error":{"details":[{ "reason" : "API_KEY_INVALID" }]}}"#;
        assert_eq!(
            classify(400, None, real),
            Outcome::KeyFailure(FailureClass::Unauthorized)
        );
    }

    fn classify_phrases(
        status: u16,
        retry_after: Option<&str>,
        body: &str,
        request_error: &[&str],
        quota: &[&str],
        rate_limit: &[&str],
    ) -> Classification {
        let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let request_error = owned(request_error);
        let quota = owned(quota);
        let rate_limit = owned(rate_limit);
        let extra = ExtraPhrases {
            request_error: &request_error,
            quota: &quota,
            rate_limit: &rate_limit,
        };
        classify_with(status, retry_after, body, UNIX_EPOCH, &extra)
    }

    #[test]
    fn extra_request_error_phrases_apply_to_400_402_and_413() {
        for status in [400, 402, 413] {
            assert_eq!(
                classify_phrases(
                    status,
                    None,
                    "too many tokens",
                    &["too many tokens"],
                    &[],
                    &[]
                )
                .outcome,
                Outcome::RequestError,
                "status {status}"
            );
        }
        for status in [401, 429, 500] {
            assert_ne!(
                classify_phrases(
                    status,
                    None,
                    "too many tokens",
                    &["too many tokens"],
                    &[],
                    &[]
                )
                .outcome,
                Outcome::RequestError,
                "request-error phrases do not apply to status {status}"
            );
        }
    }

    #[test]
    fn extra_quota_phrases_cool_the_whole_key() {
        for status in [400, 402, 413, 429, 503] {
            let result =
                classify_phrases(status, Some("30"), "plan spent", &[], &["plan spent"], &[]);
            assert_eq!(
                result.outcome,
                Outcome::KeyFailure(FailureClass::QuotaExhausted),
                "status {status}"
            );
            assert!(result.quota_phrase_matched, "status {status}");
        }
    }

    #[test]
    fn extra_rate_limit_phrases_cool_a_scope() {
        for status in [400, 402, 413, 429, 503] {
            assert_eq!(
                classify_phrases(status, Some("30"), "slow down", &[], &[], &["slow down"]).outcome,
                Outcome::KeyFailure(FailureClass::RateLimited(Some(Duration::from_secs(30)))),
                "status {status}"
            );
        }
    }

    #[test]
    fn extra_phrases_extend_built_in_lists() {
        let extra = &["unrelated"];
        assert_eq!(
            classify_phrases(402, None, "fewer max_tokens", extra, &[], &[]).outcome,
            Outcome::RequestError
        );
        assert_eq!(
            classify_phrases(400, None, "credit balance is too low", extra, &[], &[]).outcome,
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
        assert_eq!(
            classify_phrases(429, None, "free usage limit", &[], extra, &[]).outcome,
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
        assert_eq!(
            classify_phrases(503, None, "rate limit", &[], &[], extra).outcome,
            Outcome::KeyFailure(FailureClass::RateLimited(None))
        );
    }

    #[test]
    fn extra_phrases_match_without_regard_to_case() {
        assert_eq!(
            classify_phrases(400, None, "PLAN SPENT", &[], &["Plan Spent"], &[]).outcome,
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
        assert_eq!(
            classify_phrases(402, None, "TOO MANY TOKENS", &["too many tokens"], &[], &[]).outcome,
            Outcome::RequestError
        );
        assert_eq!(
            classify_phrases(429, Some("30"), "SLOW DOWN", &[], &[], &["Slow Down"]).outcome,
            Outcome::KeyFailure(FailureClass::RateLimited(Some(Duration::from_secs(30))))
        );
    }

    #[test]
    fn phrase_precedence_is_request_error_then_quota_then_rate_limit() {
        let all = "too many tokens, plan spent, slow down";
        assert_eq!(
            classify_phrases(
                402,
                None,
                all,
                &["too many tokens"],
                &["plan spent"],
                &["slow down"]
            )
            .outcome,
            Outcome::RequestError
        );
        assert_eq!(
            classify_phrases(402, None, all, &[], &["plan spent"], &["slow down"]).outcome,
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
        assert_eq!(
            classify_phrases(429, Some("30"), all, &[], &["plan spent"], &["slow down"]).outcome,
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
        assert_eq!(
            classify_phrases(429, Some("30"), "quota slow down", &[], &[], &["slow down"]).outcome,
            Outcome::KeyFailure(FailureClass::QuotaExhausted),
            "a built-in quota phrase beats an extra rate-limit phrase"
        );
    }

    #[test]
    fn only_the_first_64_kib_of_a_body_is_matched() {
        let mut before = "x".repeat(MAX_CLASSIFY_BODY);
        before.push_str("plan spent");
        assert_eq!(
            classify_phrases(400, None, &before, &[], &["plan spent"], &[]).outcome,
            Outcome::PassThrough
        );
        let mut inside = "x".repeat(MAX_CLASSIFY_BODY - "plan spent".len());
        inside.push_str("plan spent");
        assert_eq!(
            classify_phrases(400, None, &inside, &[], &["plan spent"], &[]).outcome,
            Outcome::KeyFailure(FailureClass::QuotaExhausted)
        );
    }
}
