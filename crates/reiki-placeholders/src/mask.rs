//! Replaces sensitive values in request text with stable placeholders.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::time::SystemTime;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use sha2::{Digest, Sha256};

/// Every placeholder starts with this, which the response side looks for.
pub const HEAD: &str = "[[REIKI_";
pub const TAIL: &str = "]]";
const PRIVATE_KEY_PLACEHOLDER: &str = "[[REIKI_PRIVATE_KEY_REMOVED]]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Secret,
    Term,
    PrivateKey,
}

struct Found {
    start: usize,
    end: usize,
    kind: Kind,
}

/// A placeholder depends only on the value and a per-process salt, so resent history masks identically.
pub struct Masker {
    salt: [u8; 16],
    /// One leftmost-longest automaton over the folded terms, or `None` when there are no terms.
    matcher: Option<AhoCorasick>,
    builtin: bool,
}

impl Masker {
    pub fn new(terms: &[String], builtin: bool) -> Self {
        let mut needles: Vec<String> = terms
            .iter()
            .map(|term| fold(term).into_iter().collect())
            .filter(|needle: &String| !needle.is_empty())
            .collect();
        // Terms that fold to the same text match identically, so one pattern is enough.
        needles.sort_unstable();
        needles.dedup();
        let matcher = (!needles.is_empty()).then(|| {
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::LeftmostLongest)
                .build(needles)
                .expect("aho-corasick builds from validated terms")
        });
        Self {
            salt: random_salt(),
            matcher,
            builtin,
        }
    }

    /// The masked text, or `None` if nothing matched, recording restorable originals in `reverse`.
    pub fn mask(&self, text: &str, reverse: &mut HashMap<String, String>) -> Option<String> {
        let mut found = Vec::new();
        if self.builtin {
            for span in reiki_redact::scan(text) {
                found.push(Found {
                    start: span.start,
                    end: span.end,
                    kind: if span.private_key {
                        Kind::PrivateKey
                    } else {
                        Kind::Secret
                    },
                });
            }
        }
        if let Some(matcher) = &self.matcher {
            let folded = FoldedText::new(text);
            for hit in matcher.find_iter(folded.text.as_bytes()) {
                found.push(Found {
                    start: folded.original_start(hit.start()),
                    end: folded.original_end(hit.end()),
                    kind: Kind::Term,
                });
            }
        }
        self.render(text, found, reverse)
    }

    /// Applies the earlier-then-longer rule to every match and writes the placeholders.
    fn render(
        &self,
        text: &str,
        mut found: Vec<Found>,
        reverse: &mut HashMap<String, String>,
    ) -> Option<String> {
        if found.is_empty() {
            return None;
        }

        // Earliest first, longest on ties, overlapping matches dropped.
        found.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        for item in found {
            if item.start < copied {
                continue;
            }
            out.push_str(&text[copied..item.start]);
            let original = &text[item.start..item.end];
            out.push_str(&self.placeholder(item.kind, original, reverse));
            copied = item.end;
        }
        out.push_str(&text[copied..]);
        Some(out)
    }

    fn placeholder(
        &self,
        kind: Kind,
        original: &str,
        reverse: &mut HashMap<String, String>,
    ) -> String {
        let (label, basis) = match kind {
            Kind::PrivateKey => return PRIVATE_KEY_PLACEHOLDER.to_owned(),
            Kind::Secret => ("SECRET", original.to_owned()),
            Kind::Term => ("TERM", fold(original).into_iter().collect()),
        };
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(label.as_bytes());
        hasher.update(basis.as_bytes());
        let digest = hasher.finalize();
        let id: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
        let placeholder = format!("{HEAD}{label}_{id}{TAIL}");
        reverse
            .entry(placeholder.clone())
            .or_insert_with(|| original.to_owned());
        placeholder
    }
}

fn random_salt() -> [u8; 16] {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut salt = [0u8; 16];
    for (i, chunk) in salt.chunks_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_usize(i);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    salt
}

/// Per-character lowercasing, enough to match names whatever their case.
fn fold(text: &str) -> Vec<char> {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Each character of `text` with its byte range, lowercased once for the folded haystack.
fn fold_indexed(text: &str) -> Vec<(usize, usize, char)> {
    text.char_indices()
        .map(|(i, c)| (i, i + c.len_utf8(), c.to_lowercase().next().unwrap_or(c)))
        .collect()
}

/// The folded text searched once, plus the map back to byte offsets in the original.
struct FoldedText {
    text: String,
    chars: Vec<(usize, usize, char)>,
    /// Folded byte offset where each character starts, followed by the folded length.
    starts: Vec<usize>,
}

impl FoldedText {
    fn new(text: &str) -> Self {
        let chars = fold_indexed(text);
        let mut folded = String::with_capacity(text.len());
        let mut starts = Vec::with_capacity(chars.len() + 1);
        for &(_, _, c) in &chars {
            starts.push(folded.len());
            folded.push(c);
        }
        starts.push(folded.len());
        Self {
            text: folded,
            chars,
            starts,
        }
    }

    /// The original byte offset where the character containing `offset` starts.
    fn original_start(&self, offset: usize) -> usize {
        let index = self.starts.partition_point(|&start| start < offset);
        self.chars
            .get(index)
            .map_or_else(|| self.chars.last().map_or(0, |c| c.1), |c| c.0)
    }

    /// The original byte offset just past the character ending at `offset`.
    fn original_end(&self, offset: usize) -> usize {
        let index = self.starts.partition_point(|&start| start < offset);
        index
            .checked_sub(1)
            .and_then(|i| self.chars.get(i))
            .map_or(0, |c| c.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masker(terms: &[&str], builtin: bool) -> Masker {
        let terms: Vec<String> = terms.iter().map(|t| (*t).to_owned()).collect();
        Masker::new(&terms, builtin)
    }

    /// Fixtures are assembled at runtime so no token-shaped literal sits in the source.
    fn fake(prefix: &str, tail: &str) -> String {
        [prefix, tail].concat()
    }

    #[test]
    fn terms_match_any_case_and_share_one_placeholder() {
        let m = masker(&["Acme Corp"], false);
        let mut reverse = HashMap::new();
        let out = m
            .mask("ACME corp pays acme corp and Acme Corp.", &mut reverse)
            .unwrap();
        assert!(!out.to_lowercase().contains("acme"));
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 3);
        assert_eq!(reverse.len(), 1, "one placeholder for every casing");
        assert_eq!(reverse.values().next().unwrap(), "ACME corp");
    }

    #[test]
    fn credentials_are_masked_and_restorable() {
        let m = masker(&[], true);
        let key = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        let mut reverse = HashMap::new();
        let out = m.mask(&format!("use {key} for git"), &mut reverse).unwrap();
        assert!(!out.contains(&key));
        assert!(out.starts_with("use [[REIKI_SECRET_"));
        assert_eq!(reverse.values().next().unwrap(), &key);
    }

    #[test]
    fn private_keys_are_removed_and_never_restorable() {
        let m = masker(&[], true);
        let text = "k: -----BEGIN RSA PRIVATE KEY-----\\nAAAA\\n-----END RSA PRIVATE KEY----- end";
        let mut reverse = HashMap::new();
        let out = m.mask(text, &mut reverse).unwrap();
        assert_eq!(out, "k: [[REIKI_PRIVATE_KEY_REMOVED]] end");
        assert!(reverse.is_empty());
    }

    #[test]
    fn the_same_value_gets_the_same_placeholder_and_different_values_differ() {
        let m = masker(&["Falcon", "Osprey"], false);
        let mut reverse = HashMap::new();
        let a = m.mask("Falcon and Osprey", &mut reverse).unwrap();
        let b = m.mask("Falcon again", &mut reverse).unwrap();
        let first = a.split(" and ").next().unwrap();
        assert_eq!(b.split(' ').next().unwrap(), first);
        assert_eq!(reverse.len(), 2);
    }

    #[test]
    fn text_with_nothing_sensitive_is_left_alone() {
        let m = masker(&["Falcon"], true);
        assert!(m
            .mask("an ordinary sentence", &mut HashMap::new())
            .is_none());
    }

    #[test]
    fn overlapping_matches_keep_the_longer_and_earlier_one() {
        let m = masker(&["Project Falcon", "Falcon"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("the Project Falcon plan", &mut reverse).unwrap();
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 1);
        assert_eq!(reverse.values().next().unwrap(), "Project Falcon");
    }

    #[test]
    fn non_ascii_terms_and_surroundings_are_handled() {
        let m = masker(&["Zürich Büro"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("日本 ZÜRICH büro 日本", &mut reverse).unwrap();
        assert!(out.starts_with("日本 [[REIKI_TERM_") && out.ends_with("]] 日本"));
    }

    #[test]
    fn different_maskers_use_different_salts() {
        let a = masker(&["Falcon"], false);
        let b = masker(&["Falcon"], false);
        let (mut ra, mut rb) = (HashMap::new(), HashMap::new());
        let (x, y) = (
            a.mask("Falcon", &mut ra).unwrap(),
            b.mask("Falcon", &mut rb).unwrap(),
        );
        assert_ne!(x, y);
    }

    /// The old per-term scanner, kept as a test oracle.
    fn find_folded_reference(
        chars: &[(usize, usize, char)],
        needle: &[char],
    ) -> Vec<(usize, usize)> {
        let Some(&first) = needle.first() else {
            return Vec::new();
        };
        let mut hits = Vec::new();
        let mut i = 0;
        while i + needle.len() <= chars.len() {
            if chars[i].2 == first
                && chars[i..i + needle.len()]
                    .iter()
                    .zip(needle)
                    .all(|((_, _, c), n)| c == n)
            {
                hits.push((chars[i].0, chars[i + needle.len() - 1].1));
                i += needle.len();
            } else {
                i += 1;
            }
        }
        hits
    }

    /// The old term-masking path, with the same salt and renderer as the masker under test.
    fn reference_mask(
        m: &Masker,
        terms: &[String],
        text: &str,
        reverse: &mut HashMap<String, String>,
    ) -> Option<String> {
        let folded = (!terms.is_empty()).then(|| fold_indexed(text));
        let mut found = Vec::new();
        for term in terms {
            let needle = fold(term);
            let Some(folded) = &folded else { break };
            for (start, end) in find_folded_reference(folded, &needle) {
                found.push(Found {
                    start,
                    end,
                    kind: Kind::Term,
                });
            }
        }
        m.render(text, found, reverse)
    }

    /// A small deterministic generator so the comparison test never depends on timing or entropy.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, upper: usize) -> usize {
            (self.next() % upper as u64) as usize
        }

        fn pick(&mut self, chars: &[char]) -> char {
            chars[self.below(chars.len())]
        }
    }

    /// Terms start with a character that appears nowhere else in them, so no term overlaps itself.
    /// The old per-term advance then keeps every occurrence, making it a valid oracle for the
    /// automaton's leftmost-longest scan.
    fn generated_terms(count: usize, rng: &mut Rng) -> Vec<String> {
        let middle: Vec<char> = "abcdefghijklmnoprstuvwxy".chars().collect();
        (0..count)
            .map(|i| {
                let mut term = String::from("q");
                for _ in 0..2 + rng.below(6) {
                    term.push(rng.pick(&middle));
                }
                term.push_str(&format!("{i:03}"));
                term
            })
            .collect()
    }

    fn generated_body(terms: &[String], rng: &mut Rng, target: usize) -> String {
        let filler: Vec<char> = " abcdefghijklmnopqrstuvwxyz0123456789.,;:-_"
            .chars()
            .collect();
        let mut body = String::with_capacity(target + 64);
        while body.len() < target {
            match rng.below(6) {
                0 | 1 => {
                    let term = &terms[rng.below(terms.len())];
                    if rng.below(3) == 0 {
                        body.extend(term.chars().map(|c| c.to_ascii_uppercase()));
                    } else {
                        body.push_str(term);
                    }
                }
                2 => body.push(' '),
                3 => {
                    for _ in 0..1 + rng.below(8) {
                        body.push(rng.pick(&filler));
                    }
                }
                4 => body.push('\n'),
                _ => body.push_str("lorem ipsum dolor sit amet "),
            }
        }
        body
    }

    #[test]
    fn hundreds_of_terms_over_a_large_body_match_the_old_scanner() {
        let mut rng = Rng::new(0x5eed_cafe_f00d_1234);
        let terms = generated_terms(300, &mut rng);
        let body = generated_body(&terms, &mut rng, 256 * 1024);
        let m = Masker::new(&terms, false);

        let mut got_reverse = HashMap::new();
        let got = m.mask(&body, &mut got_reverse);
        let mut want_reverse = HashMap::new();
        let want = reference_mask(&m, &terms, &body, &mut want_reverse);

        assert!(got.is_some(), "the generated body contains terms");
        assert_eq!(got, want);
        assert_eq!(got_reverse, want_reverse);
        assert!(got_reverse.len() > 100, "many terms should be exercised");
    }

    #[test]
    fn overlapping_terms_use_the_earliest_then_longest_match() {
        let m = masker(&["cdefg", "abcde"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("abcdefg", &mut reverse).unwrap();
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 1);
        assert_eq!(reverse.values().next().unwrap(), "abcde");
        assert!(out.starts_with("[[REIKI_TERM_") && out.ends_with("]]fg"));
    }

    #[test]
    fn the_longest_term_wins_at_one_start() {
        let m = masker(&["abc", "abcdef", "abcd"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("xxabcdefyy", &mut reverse).unwrap();
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 1);
        assert_eq!(reverse.values().next().unwrap(), "abcdef");
        assert!(out.starts_with("xx[[REIKI_TERM_") && out.ends_with("]]yy"));
    }

    #[test]
    fn terms_that_are_prefixes_of_each_other_mask_the_longest_fit() {
        let m = masker(&["Falcon", "Falconry", "Falconry Club"], false);
        let mut reverse = HashMap::new();
        let out = m
            .mask("Falconry Club meets Falcon and Falconry", &mut reverse)
            .unwrap();
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 3);
        let mut originals: Vec<_> = reverse.values().cloned().collect();
        originals.sort();
        assert_eq!(
            originals,
            vec!["Falcon", "Falconry", "Falconry Club"],
            "{out}"
        );
    }

    #[test]
    fn non_ascii_case_folds_that_change_byte_length_still_map_back() {
        let m = masker(&["Straße", "İstanbul", "naïve café"], false);
        let mut reverse = HashMap::new();
        let text = "A STRAẞE in İSTANBUL near 日本naïve café語 and STRASSE";
        let out = m.mask(text, &mut reverse).unwrap();
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 3, "{out}");
        let mut originals: Vec<_> = reverse.values().cloned().collect();
        originals.sort();
        assert_eq!(originals, vec!["STRAẞE", "naïve café", "İSTANBUL"]);
        assert!(out.starts_with("A [[REIKI_TERM_"), "{out}");
        assert!(out.contains("日本[[REIKI_TERM_"), "{out}");
        assert!(out.contains("]]語 and STRASSE"), "{out}");
    }

    #[test]
    fn a_term_at_the_very_start_and_end_is_masked() {
        let m = masker(&["Falcon"], false);
        let mut reverse = HashMap::new();
        let only = m.mask("Falcon", &mut reverse).unwrap();
        assert!(only.starts_with("[[REIKI_TERM_") && only.ends_with("]]"));
        assert_eq!(reverse.values().next().unwrap(), "Falcon");

        let mut reverse = HashMap::new();
        let both = m.mask("Falcon in the middle Falcon", &mut reverse).unwrap();
        assert_eq!(both.matches("[[REIKI_TERM_").count(), 2);
        assert!(both.starts_with("[[REIKI_TERM_"), "{both}");
        assert!(both.ends_with("]]"), "{both}");
    }

    #[test]
    fn a_term_spanning_multibyte_characters_keeps_its_exact_byte_span() {
        let m = masker(&["éé"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("日本éé語", &mut reverse).unwrap();
        assert!(out.starts_with("日本[[REIKI_TERM_"), "{out}");
        assert!(out.ends_with("]]語"), "{out}");
        assert_eq!(reverse.values().next().unwrap(), "éé");
    }

    #[test]
    fn an_empty_term_list_builds_no_matcher_and_masks_nothing() {
        let m = masker(&[], false);
        assert!(m.matcher.is_none());
        assert!(m.mask("anything at all", &mut HashMap::new()).is_none());
        assert!(m.mask("", &mut HashMap::new()).is_none());

        let empty = masker(&[""], false);
        assert!(empty.matcher.is_none());
        assert!(empty.mask("anything at all", &mut HashMap::new()).is_none());
    }

    /// Run with `cargo test -p reiki-placeholders --release -- --ignored five_hundred`.
    /// This is ignored so the normal suite stays quick; it checks the single pass is linear in
    /// the body size per configured term count rather than multiplying the two.
    #[test]
    #[ignore = "timing benchmark"]
    fn five_hundred_terms_over_one_mib_finishes_fast() {
        use std::time::{Duration, Instant};

        let terms: Vec<String> = (0..500)
            .map(|i| format!("q{i:03}term needs masking"))
            .collect();
        let body = "ordinary filler text without a configured word. ".repeat(24 * 1024);
        assert!(body.len() >= 1024 * 1024);
        let m = Masker::new(&terms, false);
        let started = Instant::now();
        let mut reverse = HashMap::new();
        let out = m.mask(&body, &mut reverse);
        let elapsed = started.elapsed();
        eprintln!(
            "masked {} bytes with 500 terms in {elapsed:?} (matches: {}, masked: {})",
            body.len(),
            reverse.len(),
            out.is_some()
        );
        let limit = if cfg!(debug_assertions) {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(1)
        };
        assert!(elapsed < limit, "timing regression: {elapsed:?}");
    }
}
