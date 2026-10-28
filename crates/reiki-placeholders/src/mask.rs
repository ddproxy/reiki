//! Replaces sensitive values in request text with stable placeholders.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::time::SystemTime;

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
    terms: Vec<Vec<char>>,
    builtin: bool,
}

impl Masker {
    pub fn new(terms: &[String], builtin: bool) -> Self {
        Self {
            salt: random_salt(),
            terms: terms.iter().map(|t| fold(t)).collect(),
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
        let folded = (!self.terms.is_empty()).then(|| fold_indexed(text));
        for needle in &self.terms {
            let Some(folded) = &folded else { break };
            for (start, end) in find_folded(folded, needle) {
                found.push(Found {
                    start,
                    end,
                    kind: Kind::Term,
                });
            }
        }
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

/// Each character of `text` with its byte range, lowercased once for every term to search.
fn fold_indexed(text: &str) -> Vec<(usize, usize, char)> {
    text.char_indices()
        .map(|(i, c)| (i, i + c.len_utf8(), c.to_lowercase().next().unwrap_or(c)))
        .collect()
}

/// Byte ranges of the folded text that equal `needle`.
fn find_folded(chars: &[(usize, usize, char)], needle: &[char]) -> Vec<(usize, usize)> {
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
}
