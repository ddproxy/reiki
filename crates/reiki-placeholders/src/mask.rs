//! Replaces sensitive values in request text with stable placeholders.

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::restore::{collect_strings, opaque_paths, within};

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
    /// The raw span is an escaped JSON string body, so its placeholder basis is the decoded text.
    json: bool,
}

/// A placeholder depends only on the value and a per-process salt, so resent history masks identically.
pub struct Masker {
    salt: [u8; 16],
    /// One automaton over the folded terms, scanned for overlapping matches, or `None` when there are no terms.
    matcher: Option<AhoCorasick>,
    builtin: bool,
    /// Counts explicit JSON string decodes, so a test can prove escaped-free bodies stay on the fast path.
    #[cfg(test)]
    decodes: AtomicUsize,
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
                .match_kind(MatchKind::Standard)
                .build(needles)
                .expect("aho-corasick builds from validated terms")
        });
        Self {
            salt: process_salt(),
            matcher,
            builtin,
            #[cfg(test)]
            decodes: AtomicUsize::new(0),
        }
    }

    /// The masked text, or `None` if nothing matched, recording restorable originals in `reverse`.
    pub fn mask(&self, text: &str, reverse: &mut HashMap<String, String>) -> Option<String> {
        let parsed = serde_json::from_str::<Value>(text).ok();
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
                    json: false,
                });
            }
        }
        if let Some(matcher) = &self.matcher {
            let mut strings = Vec::new();
            let mut at = 0;
            if parsed.is_some() && collect_strings(text, &mut at, "", &mut strings, 0).is_some() {
                // String values are matched on their decoded text; object keys are not scanned.
                for (start, end, _) in strings {
                    self.find_terms_in_string(matcher, text, start, end, &mut found);
                }
            } else {
                // A body that is not valid JSON keeps the old raw-text behaviour.
                self.find_terms_in_text(matcher, text, &mut found);
            }
        }
        // Reasoning signatures and base64 payloads must reach the model byte for byte.
        let protected = parsed
            .as_ref()
            .map_or_else(Vec::new, |value| protected_spans(value, text));
        found.retain(|item| {
            !protected
                .iter()
                .any(|&(start, end)| item.start >= start && item.end <= end)
        });
        self.render(text, found, reverse)
    }

    fn find_terms_in_text(&self, matcher: &AhoCorasick, text: &str, found: &mut Vec<Found>) {
        let folded = FoldedText::new(text);
        for hit in matcher.find_overlapping_iter(folded.text.as_bytes()) {
            found.push(Found {
                start: folded.original_start(hit.start()),
                end: folded.original_end(hit.end()),
                kind: Kind::Term,
                json: false,
            });
        }
    }

    /// Finds the terms in one JSON string value, mapping each decoded match back to the raw body.
    fn find_terms_in_string(
        &self,
        matcher: &AhoCorasick,
        text: &str,
        start: usize,
        end: usize,
        found: &mut Vec<Found>,
    ) {
        // `collect_strings` includes the surrounding quotes in the span.
        let body_start = start + 1;
        let body_end = end.saturating_sub(1);
        if body_end <= body_start {
            return;
        }
        let body = &text[body_start..body_end];
        if !body.contains('\\') {
            // No escapes, so the raw body is the decoded text and no decode is needed.
            let folded = FoldedText::new(body);
            for hit in matcher.find_overlapping_iter(folded.text.as_bytes()) {
                found.push(Found {
                    start: body_start + folded.original_start(hit.start()),
                    end: body_start + folded.original_end(hit.end()),
                    kind: Kind::Term,
                    json: false,
                });
            }
            return;
        }
        // No length cut-off: a caller could hide a term past it and have it sent upstream raw.
        // Request bodies are already bounded by the host, and the body was parsed above.
        #[cfg(test)]
        self.decodes.fetch_add(1, Ordering::Relaxed);
        let Ok(decoded) = serde_json::from_str::<String>(&text[start..end]) else {
            return;
        };
        let Some(raw) = RawOffsets::new(body, &decoded) else {
            return;
        };
        let folded = FoldedText::new(&decoded);
        for hit in matcher.find_overlapping_iter(folded.text.as_bytes()) {
            let decoded_start = folded.original_start(hit.start());
            let decoded_end = folded.original_end(hit.end());
            if decoded_start >= decoded_end {
                continue;
            }
            let (Some(raw_start), Some(raw_end)) = (raw.map(decoded_start), raw.map(decoded_end))
            else {
                continue;
            };
            if raw_start >= raw_end {
                continue;
            }
            found.push(Found {
                start: body_start + raw_start,
                end: body_start + raw_end,
                kind: Kind::Term,
                json: true,
            });
        }
    }

    #[cfg(test)]
    fn decode_count(&self) -> usize {
        self.decodes.load(Ordering::Relaxed)
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

        // Earliest first; overlapping matches merge into one span, so no part of either survives.
        found.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut spans: Vec<Found> = Vec::new();
        for item in found {
            match spans.last_mut() {
                Some(last) if item.start < last.end => {
                    last.end = last.end.max(item.end);
                    last.json &= item.json;
                    if item.kind == Kind::PrivateKey {
                        last.kind = Kind::PrivateKey;
                    } else if last.kind == Kind::Term && item.kind == Kind::Secret {
                        last.kind = Kind::Secret;
                    }
                }
                _ => spans.push(item),
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        for item in spans {
            out.push_str(&text[copied..item.start]);
            let original = &text[item.start..item.end];
            // An escaped term is keyed and restored by its decoded text, so both spellings agree.
            let masked = if item.json && item.kind == Kind::Term {
                match decode_string_body(original) {
                    Some(decoded) => self.placeholder(item.kind, &decoded, reverse),
                    None => self.placeholder(item.kind, original, reverse),
                }
            } else {
                self.placeholder(item.kind, original, reverse)
            };
            out.push_str(&masked);
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
        // Two values can share a truncated digest; a taken id is re-derived with the next counter so
        // no id ever stands for two originals.
        let mut counter: u32 = 0;
        loop {
            let mut hasher = Sha256::new();
            hasher.update(self.salt);
            hasher.update(label.as_bytes());
            hasher.update(counter.to_le_bytes());
            hasher.update(basis.as_bytes());
            let digest = hasher.finalize();
            let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
            let placeholder = format!("{HEAD}{label}_{id}{TAIL}");
            match reverse.get(&placeholder) {
                Some(existing) if !same_value(kind, existing, original) => counter += 1,
                Some(_) => return placeholder,
                None => {
                    reverse.insert(placeholder.clone(), original.to_owned());
                    return placeholder;
                }
            }
        }
    }
}

/// String value spans that sit inside thinking blocks or base64 image/document blocks.
fn protected_spans(parsed: &Value, text: &str) -> Vec<(usize, usize)> {
    let mut opaque = Vec::new();
    opaque_paths(parsed, "", &mut opaque);
    if opaque.is_empty() {
        return Vec::new();
    }
    let mut strings = Vec::new();
    let mut at = 0;
    if collect_strings(text, &mut at, "", &mut strings, 0).is_none() {
        return Vec::new();
    }
    strings
        .into_iter()
        .filter(|(_, _, path)| {
            opaque
                .iter()
                .any(|opaque| path == opaque || within(path, opaque))
        })
        .map(|(start, end, _)| (start, end))
        .collect()
}

/// Maps decoded byte offsets back to offsets in the raw body of a JSON string.
struct RawOffsets {
    /// `(decoded_start, raw_start, decoded_end, raw_end)` for each escape.
    escapes: Vec<(u32, u32, u32, u32)>,
}

impl RawOffsets {
    /// Walks the raw body and its decoded text in step, recording every escape boundary.
    fn new(raw: &str, decoded: &str) -> Option<Self> {
        let bytes = raw.as_bytes();
        let mut escapes = Vec::new();
        let (mut r, mut d) = (0usize, 0usize);
        while r < bytes.len() {
            if bytes[r] != b'\\' {
                // An unescaped character keeps its byte length, so both cursors move together.
                let len = raw.get(r..)?.chars().next()?.len_utf8();
                r += len;
                d += len;
                continue;
            }
            let raw_start = r;
            let decoded_start = d;
            let next = *bytes.get(r + 1)?;
            let raw_len = match next {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => 2,
                b'u' => {
                    // A four-byte decoded character can only come from a surrogate pair.
                    let ch = decoded.get(decoded_start..)?.chars().next()?;
                    if ch.len_utf8() == 4 {
                        12
                    } else {
                        6
                    }
                }
                _ => return None,
            };
            if r + raw_len > bytes.len() {
                return None;
            }
            let ch = decoded.get(decoded_start..)?.chars().next()?;
            r += raw_len;
            d += ch.len_utf8();
            escapes.push((
                u32::try_from(decoded_start).ok()?,
                u32::try_from(raw_start).ok()?,
                u32::try_from(d).ok()?,
                u32::try_from(r).ok()?,
            ));
        }
        if r != bytes.len() || d != decoded.len() {
            return None;
        }
        Some(Self { escapes })
    }

    /// The raw offset for a decoded byte offset, or `None` inside a character made by one escape.
    fn map(&self, offset: usize) -> Option<usize> {
        let offset = u32::try_from(offset).ok()?;
        let index = self
            .escapes
            .partition_point(|&(decoded_start, _, _, _)| decoded_start <= offset);
        if index == 0 {
            return Some(offset as usize);
        }
        let (decoded_start, raw_start, decoded_end, raw_end) = self.escapes[index - 1];
        if offset == decoded_start {
            Some(raw_start as usize)
        } else if offset == decoded_end {
            Some(raw_end as usize)
        } else if offset > decoded_end {
            Some((raw_end + (offset - decoded_end)) as usize)
        } else {
            None
        }
    }
}

/// Decodes the body of a JSON string (without its quotes) that already parsed as valid JSON.
fn decode_string_body(raw: &str) -> Option<String> {
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    quoted.push_str(raw);
    quoted.push('"');
    serde_json::from_str(&quoted).ok()
}

/// Whether `existing` is the original of the same value, which for a term is any casing of it.
fn same_value(kind: Kind, existing: &str, original: &str) -> bool {
    match kind {
        Kind::Term => fold(existing) == fold(original),
        _ => existing == original,
    }
}

/// One salt per process, so a config reload keeps the placeholder ids cached upstream.
fn process_salt() -> [u8; 16] {
    static SALT: OnceLock<[u8; 16]> = OnceLock::new();
    *SALT.get_or_init(|| {
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
    })
}

/// Per-character lowercasing, enough to match names whatever their case. A character whose
/// lowercase mapping expands (for example `İ` to `i` plus a combining dot) contributes every
/// resulting character.
fn fold(text: &str) -> Vec<char> {
    fold_indexed(text).into_iter().map(|(_, _, c)| c).collect()
}

/// Each folded character with the byte range of the original character it came from. An expansion
/// shares that range, so a match inside it still masks the whole original character.
fn fold_indexed(text: &str) -> Vec<(usize, usize, char)> {
    let mut out = Vec::with_capacity(text.len());
    for (start, c) in text.char_indices() {
        let end = start + c.len_utf8();
        let mut lower = c.to_lowercase();
        if let Some(first) = lower.next() {
            out.push((start, end, first));
            out.extend(lower.map(|c| (start, end, c)));
        } else {
            out.push((start, end, c));
        }
    }
    out
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
    fn overlapping_terms_and_secrets_merge_so_no_tail_escapes() {
        let m = masker(&["Project Falcon", "Falcon Heights"], false);
        let mut reverse = HashMap::new();
        let out = m
            .mask("see Project Falcon Heights now", &mut reverse)
            .unwrap();
        assert!(!out.contains("Heights") && !out.contains("Falcon"), "{out}");

        let m = masker(&["Acme-ghp"], true);
        let key = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        let mut reverse = HashMap::new();
        let out = m.mask(&format!("Acme-{key} x"), &mut reverse).unwrap();
        assert!(!out.contains("0123456789abcdef"), "{out}");
    }

    #[test]
    fn a_text_document_is_masked_but_a_base64_one_is_left_alone() {
        let m = masker(&[], true);
        let key = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        let text_doc = format!(
            r#"{{"type":"document","source":{{"type":"text","media_type":"text/plain","data":"use {key}"}}}}"#
        );
        let out = m.mask(&text_doc, &mut HashMap::new()).unwrap();
        assert!(!out.contains(&key), "{out}");
        let base64_doc = format!(
            r#"{{"type":"document","source":{{"type":"base64","media_type":"application/pdf","data":"{key}"}}}}"#
        );
        assert!(m.mask(&base64_doc, &mut HashMap::new()).is_none());
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
    fn a_lowercase_mapping_that_expands_to_several_characters_is_matched() {
        // U+0130 lowercases to the two characters `i` and U+0307. A one-character-per-byte fold
        // would keep only the first and miss the rest of the term.
        let m = masker(&["i\u{307}stanbul"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("Visit İSTANBUL now", &mut reverse).unwrap();
        assert!(!out.contains('İ') && !out.contains("STANBUL"), "{out}");
        assert_eq!(reverse.values().next().unwrap(), "İSTANBUL");

        // A match inside the expansion maps back to the whole original character.
        let m = masker(&["i"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("İ", &mut reverse).unwrap();
        assert!(!out.contains('İ'), "{out}");
        assert_eq!(reverse.values().next().unwrap(), "İ");
    }

    /// The first placeholder in a masked body.
    fn first_placeholder(text: &str) -> &str {
        let start = text.find("[[REIKI_").unwrap();
        let end = text[start..].find("]]").unwrap() + start + 2;
        &text[start..end]
    }

    #[test]
    fn an_escaped_unicode_term_gets_the_same_placeholder_as_the_raw_one() {
        let m = masker(&["Zürich"], false);
        let mut raw_reverse = HashMap::new();
        let raw = m.mask("Zürich", &mut raw_reverse).unwrap();
        let mut escaped_reverse = HashMap::new();
        let escaped = m
            .mask(r#"{"text":"Z\u00fcrich"}"#, &mut escaped_reverse)
            .unwrap();
        assert_eq!(first_placeholder(&raw), first_placeholder(&escaped));
        assert_eq!(raw_reverse.values().next().unwrap(), "Zürich");
        assert_eq!(escaped_reverse.values().next().unwrap(), "Zürich");
        assert_eq!(
            escaped,
            format!(r#"{{"text":"{}"}}"#, first_placeholder(&escaped))
        );
    }

    #[test]
    fn a_slash_escape_is_masked_as_the_decoded_term() {
        let m = masker(&["Acme/Corp"], false);
        let mut reverse = HashMap::new();
        let out = m.mask(r#"{"path":"Acme\/Corp"}"#, &mut reverse).unwrap();
        assert_eq!(reverse.values().next().unwrap(), "Acme/Corp");
        assert_eq!(out, format!(r#"{{"path":"{}"}}"#, first_placeholder(&out)));
    }

    #[test]
    fn a_term_split_by_a_unicode_escape_is_masked() {
        let m = masker(&["AcMe"], false);
        let mut reverse = HashMap::new();
        let out = m.mask(r#"{"text":"Ac\u006De"}"#, &mut reverse).unwrap();
        assert_eq!(reverse.values().next().unwrap(), "Acme");
        assert_eq!(out, format!(r#"{{"text":"{}"}}"#, first_placeholder(&out)));
    }

    #[test]
    fn a_surrogate_pair_in_a_term_maps_to_one_character() {
        let m = masker(&["Smile 😀"], false);
        let mut reverse = HashMap::new();
        let out = m
            .mask(r#"{"text":"Smile \ud83d\ude00"}"#, &mut reverse)
            .unwrap();
        assert_eq!(reverse.values().next().unwrap(), "Smile 😀");
        assert_eq!(out, format!(r#"{{"text":"{}"}}"#, first_placeholder(&out)));
    }

    #[test]
    fn a_match_spanning_an_escape_keeps_the_surrounding_bytes_exact() {
        let m = masker(&["AcMe"], false);
        let mut reverse = HashMap::new();
        let body = r#"{"a":1,"text":"pre Ac\u006De post","z":[true,null,1.50]}"#;
        let out = m.mask(body, &mut reverse).unwrap();
        let placeholder = first_placeholder(&out);
        assert_eq!(
            out,
            format!(r#"{{"a":1,"text":"pre {placeholder} post","z":[true,null,1.50]}}"#)
        );
        assert_eq!(reverse.values().next().unwrap(), "Acme");
    }

    #[test]
    fn invalid_escapes_are_left_alone_without_panicking() {
        let m = masker(&["AcMe"], false);
        for body in [
            r#"{"text":"bad \q AcMe"}"#,
            r#"{"text":"lone \ud800 AcMe"}"#,
        ] {
            let mut reverse = HashMap::new();
            let out = m.mask(body, &mut reverse).unwrap();
            assert!(
                out.contains(r"\q") || out.contains(r"\ud800"),
                "the invalid escape must stay: {out}"
            );
            assert!(!out.contains("AcMe"), "{out}");
        }
    }

    #[test]
    fn builtin_credentials_are_still_masked_in_a_json_body() {
        let m = masker(&[], true);
        let key = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        let body = format!(r#"{{"text":"use {key} now"}}"#);
        let mut reverse = HashMap::new();
        let out = m.mask(&body, &mut reverse).unwrap();
        assert!(!out.contains(&key), "{out}");
        assert_eq!(reverse.values().next().unwrap(), &key);
    }

    #[test]
    fn json_object_keys_are_out_of_scope() {
        let m = masker(&["Falcon"], false);
        let mut reverse = HashMap::new();
        let out = m.mask(r#"{"Falcon":"Falcon"}"#, &mut reverse).unwrap();
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 1, "{out}");
        assert!(out.starts_with(r#"{"Falcon":"[[REIKI_TERM_"#), "{out}");
    }

    #[test]
    fn a_json_body_without_escapes_stays_on_the_fast_path() {
        let m = masker(&["Falcon"], false);
        let mut reverse = HashMap::new();
        let out = m
            .mask(r#"{"text":"Falcon flies","n":1}"#, &mut reverse)
            .unwrap();
        assert!(out.contains("[[REIKI_TERM_"), "{out}");
        assert_eq!(m.decode_count(), 0, "no string was decoded");

        m.mask(r#"{"text":"Fal\u0063on"}"#, &mut HashMap::new())
            .unwrap();
        assert_eq!(m.decode_count(), 1, "the escaped string was decoded");
    }

    #[test]
    fn a_long_escaped_string_is_still_masked() {
        let m = masker(&["Zürich"], false);
        let body = format!(r#"{{"text":"{}Z\u00fcrich"}}"#, "x".repeat(1024 * 1024));
        assert!(body.len() > 1024 * 1024);
        let mut reverse = HashMap::new();
        let out = m.mask(&body, &mut reverse).unwrap();
        assert!(
            out.contains("[[REIKI_TERM_") && !out.contains("00fc"),
            "{out}"
        );
        assert_eq!(reverse.values().next().unwrap(), "Zürich");
        assert_eq!(m.decode_count(), 1);
    }

    #[test]
    fn maskers_in_one_process_share_the_salt_across_reloads() {
        let a = masker(&["Falcon"], false);
        let b = masker(&["Falcon"], false);
        let (mut ra, mut rb) = (HashMap::new(), HashMap::new());
        let (x, y) = (
            a.mask("Falcon", &mut ra).unwrap(),
            b.mask("Falcon", &mut rb).unwrap(),
        );
        assert_eq!(x, y, "a reload must keep the placeholder ids");
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
                    json: false,
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
    fn a_placeholder_id_is_sixty_four_bits() {
        let m = masker(&[], true);
        let mut reverse = HashMap::new();
        let p = m.placeholder(Kind::Secret, "tok-123", &mut reverse);
        let id = p
            .strip_prefix("[[REIKI_SECRET_")
            .and_then(|rest| rest.strip_suffix("]]"))
            .unwrap();
        assert_eq!(id.len(), 16, "{p}");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{p}");
    }

    #[test]
    fn an_id_taken_by_another_value_is_never_reused() {
        let m = masker(&[], true);
        let taken = m.placeholder(Kind::Secret, "value-b", &mut HashMap::new());
        let mut reverse = HashMap::from([(taken.clone(), "value-a".to_owned())]);

        let got = m.placeholder(Kind::Secret, "value-b", &mut reverse);
        assert_ne!(got, taken, "a second value must not reuse the id");
        assert_eq!(reverse[&got], "value-b");
        assert_eq!(reverse[&taken], "value-a", "the first original is kept");

        let again = m.placeholder(Kind::Secret, "value-b", &mut reverse);
        assert_eq!(again, got, "the same value keeps its placeholder");
    }

    #[test]
    fn case_variants_of_a_term_still_share_one_placeholder_after_a_collision() {
        let m = masker(&["Falcon"], false);
        let taken = m.placeholder(Kind::Term, "falcon", &mut HashMap::new());
        let mut reverse = HashMap::from([(taken.clone(), "other".to_owned())]);
        let a = m.placeholder(Kind::Term, "Falcon", &mut reverse);
        let b = m.placeholder(Kind::Term, "FALCON", &mut reverse);
        assert_eq!(a, b);
        assert_ne!(a, taken);
        assert_eq!(reverse.len(), 2);
    }

    #[test]
    fn overlapping_terms_mask_their_union() {
        let m = masker(&["cdefg", "abcde"], false);
        let mut reverse = HashMap::new();
        let out = m.mask("abcdefg", &mut reverse).unwrap();
        assert_eq!(out.matches("[[REIKI_TERM_").count(), 1);
        assert_eq!(reverse.values().next().unwrap(), "abcdefg");
        assert!(out.starts_with("[[REIKI_TERM_") && out.ends_with("]]"));
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
