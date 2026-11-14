//! Text scanning helpers shared by the detection plugins.
//!
//! A plugin keeps its own `Kind` and its own rules. This crate holds the byte-level pieces that
//! more than one plugin needs: word boundaries, keyword proximity, digit runs, a Luhn checksum and
//! the hit type. The `guard` module is the plugin shell the detection plugins share.

use http::header::CONTENT_ENCODING;
use http::HeaderMap;

pub mod guard;

/// True when the headers describe a body that is more than the clear bytes on the wire.
///
/// `Content-Encoding: identity` is a legal no-op, so a header whose every listed coding is
/// `identity` (case-insensitive, trimmed, comma separated, and repeatable) counts as absent. Any
/// other coding, an empty coding, or a value that is not visible ASCII keeps the body opaque.
pub fn body_is_encoded(headers: &HeaderMap) -> bool {
    headers.get_all(CONTENT_ENCODING).iter().any(|value| {
        let Ok(value) = value.to_str() else {
            return true;
        };
        value
            .split(',')
            .any(|coding| !coding.trim().eq_ignore_ascii_case("identity"))
    })
}

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

/// Byte ranges already covered by hits, merged and sorted so an overlap check is a binary search.
/// Rechecking every earlier hit per candidate would be quadratic on a large table.
#[derive(Debug, Default)]
pub struct Covered {
    ranges: Vec<(usize, usize)>,
}

impl Covered {
    pub fn new<K>(hits: &[Hit<K>]) -> Self {
        let mut ranges: Vec<(usize, usize)> = hits.iter().map(|h| (h.start, h.end)).collect();
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start < last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        Self { ranges: merged }
    }

    /// True when `start..end` overlaps a covered range.
    pub fn overlaps(&self, start: usize, end: usize) -> bool {
        let at = self.ranges.partition_point(|&(_, e)| e <= start);
        self.ranges.get(at).is_some_and(|&(s, _)| s < end)
    }
}

/// Adds the hits in `extra` that overlap nothing already in `hits`; exact duplicates count once.
///
/// An accepted extra also covers its bytes, so two extras over the same span, even of different
/// kinds, are not both kept. The extras are sorted by start, so one running end is enough to test
/// them against each other without a second pass over every accepted hit.
pub fn merge_hits<K: Copy>(hits: &mut Vec<Hit<K>>, mut extra: Vec<Hit<K>>) {
    let covered = Covered::new(hits);
    extra.sort_unstable_by_key(|h| (h.start, h.end));
    extra.dedup_by(|a, b| a.start == b.start && a.end == b.end);
    let mut accepted_end = None;
    hits.extend(extra.into_iter().filter(|h| {
        if covered.overlaps(h.start, h.end) || accepted_end.is_some_and(|end| h.start < end) {
            return false;
        }
        accepted_end = Some(h.end);
        true
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(start: usize, end: usize) -> Hit<u8> {
        Hit {
            kind: 0,
            start,
            end,
        }
    }

    fn hit_kind(kind: u8, start: usize, end: usize) -> Hit<u8> {
        Hit { kind, start, end }
    }

    #[test]
    fn covered_ranges_report_overlap_but_not_adjacency() {
        let covered = Covered::new(&[hit(10, 20), hit(15, 30), hit(40, 50)]);
        assert!(covered.overlaps(0, 11) && covered.overlaps(25, 26) && covered.overlaps(49, 60));
        assert!(!covered.overlaps(0, 10) && !covered.overlaps(30, 40) && !covered.overlaps(50, 60));
        assert!(!Covered::default().overlaps(0, 5));
    }

    #[test]
    fn merging_keeps_existing_hits_and_drops_overlaps_and_duplicates() {
        let mut hits = vec![hit(0, 5), hit(20, 25)];
        merge_hits(
            &mut hits,
            vec![
                hit(3, 8),
                hit(10, 15),
                hit(10, 15),
                hit(24, 30),
                hit(30, 31),
            ],
        );
        let spans: Vec<(usize, usize)> = hits.iter().map(|h| (h.start, h.end)).collect();
        assert_eq!(spans, [(0, 5), (20, 25), (10, 15), (30, 31)]);
    }

    #[test]
    fn an_accepted_extra_covers_its_bytes_for_later_extras() {
        let mut hits = Vec::new();
        merge_hits(
            &mut hits,
            vec![hit_kind(b'a', 10, 20), hit_kind(b'b', 15, 25)],
        );
        assert_eq!(hits, [hit_kind(b'a', 10, 20)], "the same span counts once");

        let mut hits = vec![hit(0, 5)];
        merge_hits(
            &mut hits,
            vec![hit_kind(b'a', 10, 20), hit_kind(b'b', 20, 30)],
        );
        assert_eq!(
            hits,
            [hit(0, 5), hit_kind(b'a', 10, 20), hit_kind(b'b', 20, 30)],
            "adjacent extras do not overlap"
        );
    }
}
