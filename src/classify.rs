use std::time::Duration;

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

pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    value?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

fn is_edge_block_page(body: &str) -> bool {
    contains_any(body, &["error code: 10"])
}

pub fn classify(status: u16, retry_after: Option<&str>, body_snippet: &str) -> Outcome {
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
                &["usage exceeded", "quota", "subscribe to go", "insufficient"],
            ) {
                Outcome::KeyFailure(FailureClass::QuotaExhausted)
            } else {
                Outcome::KeyFailure(FailureClass::RateLimited(parse_retry_after(retry_after)))
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
                Outcome::KeyFailure(FailureClass::RateLimited(parse_retry_after(retry_after)))
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
    fn rate_limit_uses_retry_after_seconds() {
        assert_eq!(
            classify(429, Some("30"), "Rate limit exceeded"),
            Outcome::KeyFailure(FailureClass::RateLimited(Some(Duration::from_secs(30))))
        );
        assert_eq!(
            classify(429, Some("Wed, 21 Oct 2026 07:28:00 GMT"), "slow down"),
            Outcome::KeyFailure(FailureClass::RateLimited(None))
        );
        assert_eq!(
            classify(429, None, ""),
            Outcome::KeyFailure(FailureClass::RateLimited(None))
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
