//! Text scanning helpers shared by the detection plugins.
//!
//! A plugin keeps its own `Kind` and its own rules. This crate holds the byte-level pieces that
//! more than one plugin needs: word boundaries, keyword proximity, digit runs, a Luhn checksum and
//! the hit type.

/// A byte range of the scanned text that looks like an identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit<K> {
    pub kind: K,
    pub start: usize,
    pub end: usize,
}

/// True when the byte at `i - 1` is part of a word.
pub fn word_before(bytes: &[u8], i: usize) -> bool {
    i > 0 && i <= bytes.len() && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')
}

/// True when the byte at `i` is part of a word.
pub fn word_at(bytes: &[u8], i: usize) -> bool {
    i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
}

/// True when one of `words` ends within `window` bytes before `at`.
pub fn keyword_before(lower: &[u8], at: usize, words: &[&str], window: usize) -> bool {
    let at = at.min(lower.len());
    let from = at.saturating_sub(window);
    let slice = &lower[from..at];
    words.iter().any(|w| {
        let w = w.as_bytes();
        slice.windows(w.len()).any(|c| c == w)
    })
}

/// The run of ASCII digits starting at `i`.
pub fn digit_run(bytes: &[u8], i: usize) -> usize {
    bytes.get(i..).map_or(0, |tail| {
        tail.iter().take_while(|b| b.is_ascii_digit()).count()
    })
}

/// The Luhn checksum over ASCII digits.
pub fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| {
            let d = u32::from(*d - b'0');
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 {
                    x - 9
                } else {
                    x
                }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}
