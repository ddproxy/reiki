//! Text scanning helpers shared by the detection plugins.
//!
//! A plugin keeps its own `Kind` and its own rules. This crate holds the byte-level pieces that
//! more than one plugin needs: word boundaries, keyword proximity, labelled values, digit runs, a
//! Luhn checksum and the hit type. The `guard` module is the plugin shell, and `columnar` the table
//! scanner, that the detection plugins share.

use http::HeaderMap;
use http::header::CONTENT_ENCODING;

pub mod columnar;
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

/// `text` with each JSON escape for whitespace or a quote (`\n`, `\t`, `\r`, `\"`, `\/`) turned
/// into a space plus the real character (or two spaces), so an identifier or banner right after
/// one is not read as part of a word and line starts are real. Byte offsets are unchanged.
pub fn blank_json_escapes(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\\') {
        return text.into();
    }
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' {
            let real = match bytes[i + 1] {
                b'n' => Some(b'\n'),
                b't' => Some(b'\t'),
                b'r' => Some(b'\r'),
                b'"' | b'/' => Some(b' '),
                _ => None,
            };
            if let Some(real) = real {
                out[i] = b' ';
                out[i + 1] = real;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).map_or_else(|_| text.into(), Into::into)
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

/// The keyword/value grammar of a detection plugin.
///
/// The plugins differ in three small ways: whether a keyword may be the prefix of a longer word,
/// whether `.` may separate the keyword from its value, and whether any ASCII whitespace may do
/// so. Passing the style keeps each caller's behaviour unchanged.
#[derive(Debug, Clone, Copy)]
pub struct KeyedStyle {
    pub allow_word_suffix: bool,
    pub dot_separator: bool,
    pub any_whitespace: bool,
}

/// Finds each keyword in `lower`, then asks `value` for the end of the identifier that follows it.
///
/// `lower` must be the ASCII-lowercase form of `bytes`. Only separators are skipped; `value`
/// decides which identifier shapes count.
pub fn keyed<K: Copy>(
    bytes: &[u8],
    lower: &[u8],
    words: &[&str],
    kind: K,
    value: fn(&[u8], usize) -> Option<usize>,
    style: KeyedStyle,
    hits: &mut Vec<Hit<K>>,
) {
    for w in words {
        let w = w.as_bytes();
        let mut from = 0;
        while let Some(at) = lower[from..]
            .windows(w.len())
            .position(|c| c == w)
            .map(|p| p + from)
        {
            from = at + w.len();
            if word_before(lower, at) || (!style.allow_word_suffix && word_at(lower, from)) {
                continue;
            }
            let mut start = from;
            let skipped = bytes[start..]
                .iter()
                .take(8)
                .take_while(|b| {
                    if style.any_whitespace && b.is_ascii_whitespace() {
                        return true;
                    }
                    (style.dot_separator && **b == b'.')
                        || matches!(**b, b' ' | b':' | b'#' | b'=' | b'"' | b'\'')
                })
                .count();
            start += skipped;
            if let Some(end) = value(bytes, start) {
                hits.push(Hit { kind, start, end });
            }
        }
    }
}

/// End of a 6 to 17 digit account number at `at`, unless it runs into a longer word.
pub fn account_value(bytes: &[u8], at: usize) -> Option<usize> {
    let n = digit_run(bytes, at);
    ((6..=17).contains(&n) && !word_at(bytes, at + n)).then_some(at + n)
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
                if x > 9 { x - 9 } else { x }
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
    #[test]
    fn json_whitespace_escapes_are_blanked_without_moving_offsets() {
        let text = r#"a\n123-45-6789\t4\\n"#;
        let blanked = super::blank_json_escapes(text);
        assert_eq!(blanked.len(), text.len());
        assert_eq!(blanked, "a \n123-45-6789 \t4\\\\n");
        assert!(matches!(
            super::blank_json_escapes("plain"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

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

    const PHI: KeyedStyle = KeyedStyle {
        allow_word_suffix: true,
        dot_separator: true,
        any_whitespace: false,
    };
    const FINANCE: KeyedStyle = KeyedStyle {
        allow_word_suffix: false,
        dot_separator: true,
        any_whitespace: false,
    };
    const CONTROLLED: KeyedStyle = KeyedStyle {
        allow_word_suffix: false,
        dot_separator: false,
        any_whitespace: true,
    };

    fn labelled<'a>(text: &'a str, words: &[&str], style: KeyedStyle) -> Vec<&'a str> {
        let lower = text.to_ascii_lowercase();
        let mut hits = Vec::new();
        keyed(
            text.as_bytes(),
            lower.as_bytes(),
            words,
            0u8,
            account_value,
            style,
            &mut hits,
        );
        hits.iter().map(|h| &text[h.start..h.end]).collect()
    }

    fn token_value(bytes: &[u8], at: usize) -> Option<usize> {
        let n = bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'-')
            .count();
        ((5..=20).contains(&n) && !word_at(bytes, at + n)).then_some(at + n)
    }

    fn tokens<'a>(text: &'a str, words: &[&str], style: KeyedStyle) -> Vec<&'a str> {
        let lower = text.to_ascii_lowercase();
        let mut hits = Vec::new();
        keyed(
            text.as_bytes(),
            lower.as_bytes(),
            words,
            0u8,
            token_value,
            style,
            &mut hits,
        );
        hits.iter().map(|h| &text[h.start..h.end]).collect()
    }

    #[test]
    fn keyed_finds_values_after_their_keyword() {
        let account = [
            "account number",
            "account_number",
            "account no",
            "account #",
            "acct",
        ];
        assert_eq!(
            labelled("account number: 000123456789", &account, PHI),
            ["000123456789"]
        );
        assert!(labelled("account number: 123", &account, PHI).is_empty());
        assert_eq!(
            labelled("acct_no 12345678", &["account", "acct_no"], FINANCE),
            ["12345678"]
        );
        assert_eq!(
            labelled("cusip 037833100", &["cusip", "cusip number"], FINANCE),
            ["037833100"]
        );
        assert_eq!(
            tokens("EIN 12-3456789", &["ein", "employer id"], CONTROLLED),
            ["12-3456789"]
        );
        assert_eq!(
            tokens("CAGE Code: 0A1B2", &["cage", "cage code"], CONTROLLED),
            ["0A1B2"]
        );
        assert_eq!(
            tokens("ECCN 5A002.a", &["eccn"], CONTROLLED),
            ["5A002"],
            "the value ends at the dot"
        );
        assert!(
            labelled("xaccount 12345678", &account, PHI).is_empty(),
            "a keyword inside a word does not count"
        );
        assert!(tokens("12-3456789", &["ein"], CONTROLLED).is_empty());
    }

    #[test]
    fn keyed_styles_keep_each_callers_separator_and_word_rules() {
        fn date_value(bytes: &[u8], at: usize) -> Option<usize> {
            let tail = &bytes[at..];
            let n = tail
                .iter()
                .take_while(|b| b.is_ascii_digit() || matches!(**b, b'/' | b'-'))
                .count();
            (n == 10).then_some(at + n)
        }
        fn dates(text: &str, style: KeyedStyle) -> Vec<&str> {
            let lower = text.to_ascii_lowercase();
            let mut hits = Vec::new();
            keyed(
                text.as_bytes(),
                lower.as_bytes(),
                &["dob", "date of birth"],
                0u8,
                date_value,
                style,
                &mut hits,
            );
            hits.iter().map(|h| &text[h.start..h.end]).collect()
        }

        assert_eq!(dates("dob.02/03/1980", PHI), ["02/03/1980"]);
        assert_eq!(dates("dob.02/03/1980", FINANCE), ["02/03/1980"]);
        assert!(
            dates("dob.02/03/1980", CONTROLLED).is_empty(),
            "controlled does not skip a dot"
        );
        assert_eq!(
            labelled("account\t12345678", &["account"], CONTROLLED),
            ["12345678"]
        );
        assert!(
            labelled("account\t12345678", &["account"], PHI).is_empty(),
            "PHI skips a space, not every ASCII whitespace"
        );
        assert_eq!(
            tokens("mrnA1234567", &["mrn"], PHI),
            ["A1234567"],
            "PHI allows a keyword to run into its value"
        );
        assert!(tokens("mrnA1234567", &["mrn"], FINANCE).is_empty());
        assert!(tokens("mrnA1234567", &["mrn"], CONTROLLED).is_empty());
    }

    #[test]
    fn account_value_checks_width_and_word_end() {
        assert_eq!(account_value(b"123456", 0), Some(6));
        assert_eq!(account_value(b"x 1234567", 2), Some(9));
        assert_eq!(account_value(b"12345", 0), None);
        assert_eq!(account_value(b"123456789012345678", 0), None);
        assert_eq!(account_value(b"1234567x", 0), None);
    }
}
