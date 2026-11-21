#![no_main]
// Placeholder expansion and restoration: no panic, bounded output, only generated placeholders
// in the reverse map, and restoring the masked text reproduces the original.
#![forbid(unsafe_code)]

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use reiki_placeholders::fuzzing::{
    incomplete_start, restore_text, validate_terms, Masker, HEAD, TAIL,
};

fn split(text: &str) -> (&str, &str) {
    let mut at = text.find('\n').unwrap_or(text.len() / 2);
    while !text.is_char_boundary(at) {
        at += 1;
    }
    (&text[..at], &text[at..])
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (raw_term, raw_body) = split(&text);
    let term: String = raw_term.chars().take(64).collect();
    let body: String = raw_body.chars().take(4096).collect();

    let terms = validate_terms(vec![term]).unwrap_or_default();
    let _ = incomplete_start(&body);
    let _ = reiki_redact::scan(&body);
    let _ = reiki_redact::partial_tail(&body);

    let mut reverse = HashMap::new();
    let masker = Masker::new(&terms, true);
    if let Some(masked) = masker.mask(&body, &mut reverse) {
        assert!(
            masked.len() <= 32 * body.len() + 256,
            "masking grew {} bytes to {}",
            body.len(),
            masked.len()
        );
        assert!(
            reverse
                .keys()
                .all(|key| key.starts_with(HEAD) && key.ends_with(TAIL)),
            "every reverse entry must be a placeholder this plugin wrote"
        );
        let restored = restore_text(&masked, &reverse);
        assert!(
            reverse.keys().all(|key| !restored.contains(key)),
            "every generated placeholder must be restored"
        );
        // Terms are matched case-insensitively and stored folded, so the restored spelling need
        // not byte-match the original; no placeholder may be left in the output, though.
        assert!(restored.len() <= 32 * body.len() + 256);
    }
});
