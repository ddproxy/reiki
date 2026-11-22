#![no_main]
// Secret-form expansion and stream scrubbing: no panic, bounded forms, same-length output, and
// the configured secret never survives in the scrubbed text.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use reiki::scrub::{StreamScrubber, contains_key_material, forms, scrub_text};

// Long keys are covered by the crate's unit tests; a shorter bound keeps libFuzzer's throughput
// useful while still crossing the eight-byte minimum and every form family's budget.
const MAX_FUZZ_SECRET: usize = 48;

fn split(text: &str) -> (&str, &str) {
    let mut at = text.find('\n').unwrap_or(text.len() / 2);
    while !text.is_char_boundary(at) {
        at += 1;
    }
    (&text[..at], &text[at..])
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (raw_secret, raw_body) = split(&text);

    let mut end = raw_secret.len().min(MAX_FUZZ_SECRET);
    while !raw_secret.is_char_boundary(end) {
        end -= 1;
    }
    let secret = &raw_secret[..end];
    let body: String = raw_body.chars().take(4096).collect();
    let secrets = [secret];

    let expanded = forms(&secrets);
    assert!(
        expanded
            .windows(2)
            .all(|pair| pair[0].len() >= pair[1].len()),
        "forms must be ordered longest first"
    );
    assert!(
        expanded
            .iter()
            .all(|form| !form.is_empty() && form.len() <= 16 * secret.len() + 128),
        "a form must stay bounded by the key length"
    );
    if secret.len() >= 8 {
        assert!(
            expanded
                .iter()
                .any(|form| form.as_slice() == secret.as_bytes()),
            "the raw form must always be present"
        );
    }

    let scrubbed = scrub_text(&body, &secrets);
    assert_eq!(scrubbed.len(), body.len(), "scrubbing must preserve length");
    if secret.chars().count() >= 8 {
        assert!(!scrubbed.contains(secret), "the key survived scrub_text");
    }
    let _ = contains_key_material(&scrubbed, &secrets);

    if let Some(mut scrubber) = StreamScrubber::new(&secrets) {
        let mut out = Vec::with_capacity(body.len());
        for piece in body.as_bytes().chunks(7) {
            out.extend_from_slice(&scrubber.feed(piece, false));
        }
        out.extend_from_slice(&scrubber.feed(&[], true));
        assert_eq!(out.len(), body.len(), "the stream must preserve length");
        let streamed = String::from_utf8_lossy(&out);
        if secret.chars().count() >= 8 {
            assert!(
                !streamed.contains(secret),
                "the key survived the stream scrubber"
            );
        }
    }
});
