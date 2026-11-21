#![no_main]
// The 400 invalid-key marker must come from a real JSON property, not from text that happens to
// contain the pair. Compare the production matcher with a direct walk of the parsed document.
#![forbid(unsafe_code)]

use std::time::UNIX_EPOCH;

use libfuzzer_sys::fuzz_target;
use reiki::classify::{classify_at, parse_retry_after};
use reiki::fuzzing::has_json_pair;
use serde_json::Value;

fn find(json: &Value, field: &str, value: &str) -> bool {
    match json {
        Value::Object(map) => map.iter().any(|(key, child)| {
            (key == field && child.as_str() == Some(value)) || find(child, field, value)
        }),
        Value::Array(items) => items.iter().any(|child| find(child, field, value)),
        _ => false,
    }
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut at = text.find('\n').unwrap_or(text.len() / 2);
    while !text.is_char_boundary(at) {
        at += 1;
    }
    let body: String = text[..at].chars().take(4096).collect();
    let tail: String = text[at..].chars().take(32).collect();
    let field = if tail.is_empty() { "reason" } else { &tail };
    let mut mid = tail.len() / 2;
    while !tail.is_char_boundary(mid) {
        mid += 1;
    }
    let value = if tail.len() < 2 {
        "api_key_invalid"
    } else {
        &tail[mid..]
    };

    let expected = serde_json::from_str::<Value>(&body)
        .ok()
        .is_some_and(|json| find(&json, field, value));
    assert_eq!(
        has_json_pair(&body, field, value),
        expected,
        "the JSON property matcher disagreed with a direct document walk"
    );

    let _ = classify_at(400, Some(value), &body, UNIX_EPOCH);
    let _ = parse_retry_after(Some(value), UNIX_EPOCH);
});
