use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    RateLimited(Option<Duration>),
    QuotaExhausted,
    AccessDisabled,
    Unauthorized,
    Transient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    KeyFailure(FailureClass),
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
fn parse_http_date(value: &str) -> Option<i64> {
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

// Inverse of `days_from_civil` (Howard Hinnant's civil_from_days).
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

fn is_edge_block_page(body: &str) -> bool {
    contains_any(body, &["error code: 10"])
}

pub fn classify(status: u16, retry_after: Option<&str>, body_snippet: &str) -> Outcome {
    classify_at(status, retry_after, body_snippet, SystemTime::now())
}

pub fn classify_at(
    status: u16,
    retry_after: Option<&str>,
    body_snippet: &str,
    now: SystemTime,
) -> Outcome {
    let body = body_snippet.to_ascii_lowercase();
    match status {
        200..=399 => Outcome::Success,
        401 => Outcome::KeyFailure(FailureClass::Unauthorized),
        402 => Outcome::KeyFailure(FailureClass::QuotaExhausted),
        400 => {
            if contains_any(&body, &["api key not valid", "api_key_invalid"]) {
                Outcome::KeyFailure(FailureClass::Unauthorized)
            } else if contains_any(&body, &["credit balance is too low"]) {
                Outcome::KeyFailure(FailureClass::QuotaExhausted)
            } else {
                Outcome::PassThrough
            }
        }
        429 => {
            if contains_any(
                &body,
                &[
                    "usage exceeded",
                    "quota",
                    "subscribe to go",
                    "free usage limit",
                    "insufficient",
                ],
            ) {
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
            } else if contains_any(&body, &["access is disabled", "free tier", "not enabled"]) {
                Outcome::KeyFailure(FailureClass::AccessDisabled)
            } else {
                Outcome::KeyFailure(FailureClass::Unauthorized)
            }
        }
        500..=599 => {
            // Some upstreams report an exhausted allowance as a 5xx with a body.
            if contains_any(&body, &["model access is disabled", "access is disabled"]) {
                Outcome::KeyFailure(FailureClass::AccessDisabled)
            } else if contains_any(&body, &["rate limit"]) {
                Outcome::KeyFailure(FailureClass::RateLimited(parse_retry_after(
                    retry_after,
                    now,
                )))
            } else {
                Outcome::KeyFailure(FailureClass::Transient)
            }
        }
        _ => Outcome::PassThrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            classify(403, None, "forbidden"),
            Outcome::KeyFailure(FailureClass::Unauthorized)
        );
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
}
