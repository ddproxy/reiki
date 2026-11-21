#![no_main]
// Tunnel allow-list parsing: no panic, only valid rules can allow a target, and canonicalised
// hosts stay bounded by the rule text.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use reiki::fuzzing::split_host_port;
use reiki::tunnel::{allowed, valid_rule};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let rule = text.as_ref();

    let valid = valid_rule(rule);
    let entries = [rule.to_owned()];
    assert!(
        !allowed(&entries, rule, 443) || valid,
        "an invalid rule must never allow a target"
    );
    if let Some((host, port)) = split_host_port(rule, Some(443)) {
        // A compressed IPv6 literal can grow when canonicalised, and lossy UTF-8 decoding can
        // expand invalid bytes up to 3x, so allow for either the full form or the input length.
        assert!(!host.is_empty() && host.len() <= text.len().max(64));
        let _ = port;
    }

    assert!(valid_rule("example.com"));
    assert!(!valid_rule(""));
    assert!(allowed(&["example.com".to_owned()], "example.com", 443));
    assert!(!allowed(&["example.com".to_owned()], "example.com", 80));
});
