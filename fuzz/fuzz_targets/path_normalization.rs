#![no_main]
// Routing and admin path decoding: no panic, decoded output never longer than the input, slices
// stay inside the input, and known traversal spellings are always refused.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use reiki::fuzzing::{
    decode_path_layer, decode_query_value, has_path_trick, parse_dry_run, parse_key_query,
    split_upstream,
};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let path = text.as_ref();

    if let Some(decoded) = decode_path_layer(path) {
        assert!(
            decoded.len() <= path.len(),
            "percent-decoding cannot grow a path"
        );
    }
    let _ = has_path_trick(path);
    if let Some((upstream, rest)) = split_upstream(path) {
        assert!(path.contains(&upstream) && path.contains(&rest));
    }
    if let Some(decoded) = decode_query_value(path) {
        assert!(
            decoded.len() <= path.len(),
            "a query value cannot grow when decoded"
        );
    }
    if let Ok(key) = parse_key_query(&format!("key={path}")) {
        assert!(key.len() <= path.len());
    }
    let _ = parse_dry_run(&format!("dry_run={path}"));

    assert!(has_path_trick("/a/%2e%2e/b"));
    assert!(has_path_trick("/a/%252e%252e/b"));
    assert!(has_path_trick("/a/%2Fb"));
    assert!(!has_path_trick("/a/b"));
});
