#![no_main]
// TOML config parsing: no panic, and a document that parses yields no more keys, upstreams,
// streams or plugins than it has lines to describe them.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;
use reiki::config::Config;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Ok(config) = Config::parse(&text) {
        let lines = text.lines().count().max(1);
        assert!(
            !config.listen.is_empty(),
            "a parsed config needs a listen address"
        );
        assert!(config.keys.len() <= lines, "more keys than config lines");
        assert!(
            config.upstream.len() <= lines,
            "more upstreams than config lines"
        );
        assert!(
            config.streams.len() <= lines,
            "more streams than config lines"
        );
        assert!(
            config.plugins.len() <= lines,
            "more plugins than config lines"
        );
    }
});
