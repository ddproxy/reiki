#![no_main]
// The SSE parser turns untrusted peer-stream bytes into events: no panic, and the emitted
// bytes and item count stay bounded by what was fed in.
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // A small, input-derived cap exercises the boundary checks without needing 64 KiB inputs.
    let cap = 16 + usize::from(data[0]) % 512;
    let first = data.len() / 3;
    let second = data.len() * 2 / 3;
    let chunks: [&[u8]; 4] = [&data[..first], &data[first..second], &data[second..], &[]];

    let outcome = reiki::fuzzing::sse_feed(&chunks, cap).unwrap_or_default();
    assert!(
        outcome.items <= reiki::fuzzing::MAX_SSE_ITEMS_PER_FEED * chunks.len(),
        "one feed may not emit more than the item cap"
    );
    // Lossy UTF-8 can expand a byte to at most three bytes, and each extra data line adds one
    // newline. The slack covers the accumulated event and data buffers across all chunks.
    assert!(
        outcome.bytes <= 8 * data.len() + 64,
        "emitted {} bytes from {} input bytes",
        outcome.bytes,
        data.len()
    );
});
