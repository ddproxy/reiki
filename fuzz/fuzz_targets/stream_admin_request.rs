#![no_main]
// Stream-admin request decoding: arbitrary bytes must not panic, and the strict DTOs must keep
// rejecting unknown properties. The production DTOs are private, so the fuzzing wrapper feeds
// them directly.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    reiki::fuzzing::fuzz_stream_admin_request(data);
});
