#![no_main]
// Key-file validation and key-status parsing: no panic, accepted key material stays within the
// scrubber's bounds, and its raw form is always one of the forms the scrubber will blank.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use reiki::fuzzing::key_file_verdict;
use reiki::keys::parse_status;
use reiki::scrub::{forms, MAX_SECRET};

fn split(text: &str) -> (&str, &str) {
    let mut at = text.find('\n').unwrap_or(text.len() / 2);
    while !text.is_char_boundary(at) {
        at += 1;
    }
    (&text[..at], &text[at..])
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (raw_key, flags) = split(&text);
    let mode = flags.as_bytes().first().map(|byte| u32::from(*byte));
    let strict = flags.as_bytes().get(1).is_some_and(|byte| byte % 2 == 0);

    if let Ok(value) = key_file_verdict("fuzz", raw_key, mode, strict) {
        assert_eq!(value, raw_key.trim());
        assert!((8..=MAX_SECRET).contains(&value.len()));
        assert!(
            forms(&[&value])
                .iter()
                .any(|form| form.as_slice() == value.as_bytes()),
            "an accepted key must have a raw scrubber form"
        );
    }

    let _ = parse_status(&text);
});
