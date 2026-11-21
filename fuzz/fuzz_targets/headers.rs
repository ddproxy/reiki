#![no_main]
// Header and query credential parsers: no panic, tokens and stripped queries stay bounded by the
// header text, and parsed resets stay under the shared 24-hour cap.
#![forbid(unsafe_code)]

use std::str::FromStr;
use std::time::{Duration, UNIX_EPOCH};

use http::{HeaderMap, HeaderName, HeaderValue};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let value = text.as_ref();

    if let Some(token) = reiki::fuzzing::bearer_token(value) {
        assert!(token.len() <= value.len());
        assert!(value.contains(token));
    }
    if let Some(token) = reiki::auth::proxy_token(value) {
        assert!(token.len() <= 2 * value.len() + 64);
    }
    if let Some(query) = reiki::auth::strip_query_key(value) {
        assert!(query.len() <= value.len());
    }

    let mut headers = HeaderMap::new();
    for name in [
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "x-goog-api-key",
        "retry-after",
        "date",
        "x-ratelimit-limit-requests",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset",
        "anthropic-ratelimit-requests-reset",
        "content-encoding",
    ] {
        if let (Ok(name), Ok(header_value)) =
            (HeaderName::from_str(name), HeaderValue::from_str(value))
        {
            headers.insert(name, header_value);
        }
    }

    let _ = reiki::auth::extract_token(&headers, Some(value));
    let _ = reiki_detect::body_is_encoded(&headers);
    let _ = reiki::shaper::headers::Preset::parse(value);
    let _ = reiki::fuzzing::parse_duration(value);

    let observed = reiki::fuzzing::observe_at(&headers, 0);
    for wait in [
        observed.requests_reset,
        observed.tokens_reset,
        observed.retry_after,
    ]
    .into_iter()
    .flatten()
    {
        assert!(wait <= Duration::from_secs(24 * 60 * 60));
    }
    if let Some(wait) = reiki::classify::parse_retry_after(Some(value), UNIX_EPOCH) {
        assert!(wait <= Duration::from_secs(24 * 60 * 60));
    }
    assert!(reiki::auth::constant_time_eq(value, value));
});
