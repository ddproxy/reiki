#![no_main]
// Request-log and credential redaction: no panic, bounded output, and no credential-shaped span
// left in text that went through `secrets_cut`.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use reiki_requestlog::fuzzing::{form, header, json, path, secrets_cut, sse_events, stream_deltas};
use serde_json::{json, Value};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut end = text.len().min(4096);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let text = &text[..end];

    let masked = secrets_cut(text, true);
    assert!(
        masked.len() <= 32 * text.len() + 256,
        "redaction grew {} bytes to {}",
        text.len(),
        masked.len()
    );
    assert!(
        reiki_redact::scan(&masked).is_empty(),
        "a credential-shaped span survived redaction"
    );

    let _ = form(text, true);
    let masked_path = path(text);
    assert!(masked_path.len() <= 32 * text.len() + 256);
    let _ = header("authorization", text);
    let _ = header("x-api-key", text);
    let _ = sse_events(text);

    let mut value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    json(&mut value);
    let mut events = vec![
        json!({ "choices": [{ "delta": { "content": text } }] }),
        json!({ "choices": [{ "delta": { "content": "" } }] }),
    ];
    stream_deltas(&mut events, true);
});
