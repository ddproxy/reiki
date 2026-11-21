//! Removes an upstream key that a successful response echoes back, as the body streams.
//!
//! A key can arrive split across two chunks, so the scrubber keeps back only the tail that could
//! still become the start of a key. That is usually nothing, so a server-sent event is not held
//! up. The replacement has the length of what it replaces, so a response with a content length
//! keeps a correct one.

use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::Bytes;
use futures_core::Stream;

/// A secret shorter than this is more likely ordinary text than a key. `load_keys` refuses a
/// configured key below it: an echoed key that short would never be scrubbed.
pub(crate) const MIN_SECRET: usize = 8;

type ByteResult = Result<Bytes, std::io::Error>;

/// A configured key longer than this is refused at load time. Every form of a key is at least
/// as long as the key, so an unbounded key would make the scrubber's bounded form list unbounded
/// in size too.
pub const MAX_SECRET: usize = 4096;

/// A key split at every character boundary for every ASCII whitespace and control byte needs
/// one form per boundary per separator. The cap keeps an unusually long credential bounded.
const MAX_FORMS_PER_SECRET: usize = 4096;

/// Reserved slots for each family of forms, so none can crowd the others out of a long key's
/// list. The sum is the whole per-secret budget; each generator stops as soon as its slots run out.
const RAW_FORM_BUDGET: usize = 1;
const SPLIT_FORM_BUDGET: usize = MAX_FORMS_PER_SECRET / 2;
const JSON_FORM_BUDGET: usize = MAX_FORMS_PER_SECRET / 8;
const PERCENT_FORM_BUDGET: usize = MAX_FORMS_PER_SECRET / 8;
const B64_FORM_BUDGET: usize = MAX_FORMS_PER_SECRET
    - RAW_FORM_BUDGET
    - SPLIT_FORM_BUDGET
    - JSON_FORM_BUDGET
    - PERCENT_FORM_BUDGET;

/// Adds one form unless the shared slot budget is spent.
fn push_form(forms: &mut Vec<Vec<u8>>, budget: usize, form: Vec<u8>) -> bool {
    if forms.len() >= budget {
        return false;
    }
    forms.push(form);
    true
}

/// Every byte string that stands for one of `secrets`, longest first.
pub fn forms(secrets: &[&str]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for secret in secrets.iter().filter(|s| s.len() >= MIN_SECRET) {
        let raw = secret.as_bytes();
        let mut per_secret: Vec<Vec<u8>> = Vec::with_capacity(MAX_FORMS_PER_SECRET);
        per_secret.push(raw.to_vec());
        split_forms(secret, &mut per_secret, RAW_FORM_BUDGET + SPLIT_FORM_BUDGET);
        let json_limit = per_secret.len() + JSON_FORM_BUDGET;
        json_forms(secret, &mut per_secret, json_limit);
        let percent_limit = per_secret.len() + PERCENT_FORM_BUDGET;
        percent_forms(raw, &mut per_secret, percent_limit);
        percent_all_forms(raw, &mut per_secret, percent_limit);
        percent_unreserved_forms(raw, &mut per_secret, percent_limit);
        percent_one_forms(raw, &mut per_secret, percent_limit);
        let b64_limit = per_secret.len() + B64_FORM_BUDGET;
        base64_forms(raw, &mut per_secret, b64_limit);
        // Longest first: blanking a short form inside a longer one would leave the rest of the
        // longer form visible. Dedupe before the single truncation so no family is lost to copies.
        per_secret.sort_by_key(|f| std::cmp::Reverse(f.len()));
        let mut seen = std::collections::HashSet::new();
        per_secret.retain(|f| seen.insert(f.clone()));
        per_secret.truncate(MAX_FORMS_PER_SECRET);
        out.extend(per_secret);
    }
    // The formatted credential contains the raw key, so the longer one goes first.
    out.sort_by_key(|f| std::cmp::Reverse(f.len()));
    let mut seen = std::collections::HashSet::new();
    out.retain(|f| seen.insert(f.clone()));
    out
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64(data: &[u8], alphabet: &[u8; 64], padded: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let n = (u32::from(group[0]) << 16)
            | (u32::from(*group.get(1).unwrap_or(&0)) << 8)
            | u32::from(*group.get(2).unwrap_or(&0));
        for i in 0..=group.len() {
            out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        if padded {
            for _ in group.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

fn push_unicode_escape(out: &mut String, c: char, upper: bool) {
    let mut units = [0u16; 2];
    for unit in c.encode_utf16(&mut units) {
        if upper {
            out.push_str(&format!("\\u{unit:04X}"));
        } else {
            out.push_str(&format!("\\u{unit:04x}"));
        }
    }
}

fn push_minimal_json(out: &mut String, c: char, slash: bool) {
    match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '/' if slash => out.push_str("\\/"),
        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
        c => out.push(c),
    }
}

/// Minimal escapes, with `\/`, with every non-alphanumeric as `\uXXXX`, with every character
/// as `\uXXXX`, and with every alphanumeric as `\uXXXX`, in either hex case. A single escaped
/// letter or digit at each position is also covered, since JSON encoders make that choice
/// independently.
fn json_forms(text: &str, forms: &mut Vec<Vec<u8>>, budget: usize) -> bool {
    for style in 0..8 {
        let mut s = String::new();
        for c in text.chars() {
            let escape = match style {
                2 | 3 => !c.is_ascii_alphanumeric(),
                4 | 5 => true,
                6 | 7 => c.is_ascii_alphanumeric(),
                _ => false,
            };
            if escape {
                push_unicode_escape(&mut s, c, matches!(style, 3 | 5 | 7));
            } else {
                push_minimal_json(&mut s, c, style == 1);
            }
        }
        if s != text && !push_form(forms, budget, s.into_bytes()) {
            return false;
        }
    }
    for (index, escaped) in text
        .chars()
        .enumerate()
        .filter(|(_, c)| c.is_ascii_alphanumeric())
    {
        for upper in [false, true] {
            let mut s = String::new();
            for (i, c) in text.chars().enumerate() {
                if i == index {
                    push_unicode_escape(&mut s, escaped, upper);
                } else {
                    push_minimal_json(&mut s, c, false);
                }
            }
            if !push_form(forms, budget, s.into_bytes()) {
                return false;
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for escaped in text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() && seen.insert(*c))
    {
        for upper in [false, true] {
            let mut s = String::new();
            for c in text.chars() {
                if c == escaped {
                    push_unicode_escape(&mut s, c, upper);
                } else {
                    push_minimal_json(&mut s, c, false);
                }
            }
            if !push_form(forms, budget, s.into_bytes()) {
                return false;
            }
        }
    }
    true
}

fn percent_byte(out: &mut Vec<u8>, b: u8, upper: bool) {
    let hex: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    out.extend_from_slice(&[b'%', hex[usize::from(b >> 4)], hex[usize::from(b & 15)]]);
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// Lower and upper case hex, and a space as `+`.
fn percent_forms(raw: &[u8], forms: &mut Vec<Vec<u8>>, budget: usize) -> bool {
    for (upper, plus) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut out = Vec::with_capacity(raw.len());
        for &b in raw {
            if is_unreserved(b) {
                out.push(b);
            } else if plus && b == b' ' {
                out.push(b'+');
            } else {
                percent_byte(&mut out, b, upper);
            }
        }
        if out != raw && !push_form(forms, budget, out) {
            return false;
        }
    }
    true
}

/// Every byte percent-encoded, as a client that encodes even unreserved characters would.
fn percent_all_forms(raw: &[u8], forms: &mut Vec<Vec<u8>>, budget: usize) -> bool {
    for upper in [true, false] {
        let mut out = Vec::with_capacity(raw.len() * 3);
        for &b in raw {
            percent_byte(&mut out, b, upper);
        }
        if !push_form(forms, budget, out) {
            return false;
        }
    }
    true
}

/// Only the unreserved characters percent-encoded, leaving reserved bytes as they are.
fn percent_unreserved_forms(raw: &[u8], forms: &mut Vec<Vec<u8>>, budget: usize) -> bool {
    for upper in [true, false] {
        let mut out = Vec::with_capacity(raw.len());
        for &b in raw {
            if is_unreserved(b) {
                percent_byte(&mut out, b, upper);
            } else {
                out.push(b);
            }
        }
        if !push_form(forms, budget, out) {
            return false;
        }
    }
    true
}

/// One unreserved character percent-encoded at each position, in either hex case.
fn percent_one_forms(raw: &[u8], forms: &mut Vec<Vec<u8>>, budget: usize) -> bool {
    for (i, &b) in raw.iter().enumerate().filter(|(_, b)| is_unreserved(**b)) {
        for upper in [true, false] {
            let mut out = Vec::with_capacity(raw.len() + 2);
            out.extend_from_slice(&raw[..i]);
            percent_byte(&mut out, b, upper);
            out.extend_from_slice(&raw[i + 1..]);
            if !push_form(forms, budget, out) {
                return false;
            }
        }
    }
    true
}

/// The complete base64 characters a key contributes when it starts at `offset` inside a larger
/// string. `offset` is the number of unknown bytes before the key; the leading and trailing
/// characters that depend on those bytes (or on an unknown suffix) are trimmed.
fn base64_at_offset(raw: &[u8], offset: usize, alphabet: &[u8; 64]) -> Option<Vec<u8>> {
    let total = offset + raw.len();
    let start = offset + 1;
    let end = total / 3 * 4
        + match total % 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => unreachable!(),
        };
    if end <= start {
        return None;
    }
    let mut data = vec![0; offset];
    data.extend_from_slice(raw);
    let encoded = base64(&data, alphabet, false);
    Some(encoded.as_bytes()[start..end].to_vec())
}

/// The key with one ASCII whitespace or control byte inserted at each boundary, plus a form
/// with one between every character. Forms are built only while slots remain: a long key would
/// otherwise allocate every separator times every boundary before the budget was applied.
fn split_forms(text: &str, forms: &mut Vec<Vec<u8>>, budget: usize) -> bool {
    let chars: Vec<char> = text.chars().collect();
    // A space is by far the most common separator injected by an echoing service, so its forms
    // come first and survive a budget that cannot hold every separator for a very long key.
    let separators = std::iter::once(b' ').chain(
        (0u8..=0x20)
            .chain(std::iter::once(0x7f))
            .filter(|b| *b != b' '),
    );
    for byte in separators {
        if forms.len() >= budget {
            return false;
        }
        let separator = char::from(byte);
        let mut joined = String::with_capacity(text.len() + chars.len());
        for (i, c) in chars.iter().enumerate() {
            if i > 0 {
                joined.push(separator);
            }
            joined.push(*c);
        }
        if !push_form(forms, budget, joined.into_bytes()) {
            return false;
        }
        for split in 1..chars.len() {
            let mut form = String::with_capacity(text.len() + separator.len_utf8());
            for (i, c) in chars.iter().enumerate() {
                if i == split {
                    form.push(separator);
                }
                form.push(*c);
            }
            if !push_form(forms, budget, form.into_bytes()) {
                return false;
            }
        }
    }
    true
}

/// Base64 forms: the plain encodings and offset alignments first, then their escapes. The plain
/// forms are what an echoing service most often emits, so the slot budget must reach them first.
fn base64_forms(raw: &[u8], forms: &mut Vec<Vec<u8>>, budget: usize) {
    let mut encoded = Vec::new();
    for alphabet in [B64_STD, B64_URL] {
        for padded in [true, false] {
            encoded.push(base64(raw, alphabet, padded));
        }
    }
    for text in &encoded {
        if !push_form(forms, budget, text.as_bytes().to_vec()) {
            return;
        }
    }
    for alphabet in [B64_STD, B64_URL] {
        for offset in [1, 2] {
            if let Some(form) = base64_at_offset(raw, offset, alphabet) {
                if !push_form(forms, budget, form) {
                    return;
                }
            }
        }
    }
    for text in &encoded {
        if !json_forms(text, forms, budget) || !percent_forms(text.as_bytes(), forms, budget) {
            return;
        }
    }
}

pub struct StreamScrubber {
    secrets: Vec<Vec<u8>>,
    /// Reversed proper prefixes of every form, so a walk from the end of a chunk finds the
    /// longest possible partial secret without scanning the whole form list per chunk.
    prefixes: PrefixTrie,
    carry: Vec<u8>,
}

/// A trie of reversed prefixes. A node marks the end of a prefix only when an inserted prefix
/// finishes there: intermediate nodes are just steps toward a longer prefix.
struct PrefixTrie {
    /// Index of the first edge leaving each node, or `NO_EDGE` when it has none.
    heads: Vec<usize>,
    /// Whether the path to this node is a whole proper prefix of a secret form.
    terminal: Vec<bool>,
    edges: Vec<TrieEdge>,
    /// Counts one step per byte walked, so a test can prove the suffix scan stays linear.
    #[cfg(test)]
    suffix_steps: AtomicUsize,
}

struct TrieEdge {
    byte: u8,
    target: usize,
    next: usize,
}

const NO_EDGE: usize = usize::MAX;

impl PrefixTrie {
    fn new(secrets: &[Vec<u8>]) -> Self {
        let mut trie = Self {
            heads: vec![NO_EDGE],
            terminal: vec![false],
            edges: Vec::new(),
            #[cfg(test)]
            suffix_steps: AtomicUsize::new(0),
        };
        for secret in secrets {
            // A full form is blanked before the suffix check. Every shorter prefix is a possible
            // held-back tail, so each one is inserted in reverse for the backward walk.
            for len in 1..secret.len() {
                trie.insert(&secret[..len]);
            }
        }
        trie
    }

    /// Inserts a secret prefix in reverse, so the trie can be walked backwards from a chunk end.
    fn insert(&mut self, prefix: &[u8]) {
        let mut node = 0;
        for &byte in prefix.iter().rev() {
            let mut edge = self.heads[node];
            while edge != NO_EDGE && self.edges[edge].byte != byte {
                edge = self.edges[edge].next;
            }
            if edge == NO_EDGE {
                let target = self.heads.len();
                self.heads.push(NO_EDGE);
                self.terminal.push(false);
                let added = self.edges.len();
                self.edges.push(TrieEdge {
                    byte,
                    target,
                    next: self.heads[node],
                });
                self.heads[node] = added;
                node = target;
            } else {
                node = self.edges[edge].target;
            }
        }
        self.terminal[node] = true;
    }

    /// Length of the longest suffix of `data` that is a proper prefix of a secret form.
    fn longest_suffix(&self, data: &[u8]) -> usize {
        let mut node = 0;
        let mut keep = 0;
        for (length, &byte) in data.iter().rev().enumerate() {
            #[cfg(test)]
            self.suffix_steps.fetch_add(1, Ordering::Relaxed);
            let mut edge = self.heads[node];
            while edge != NO_EDGE && self.edges[edge].byte != byte {
                edge = self.edges[edge].next;
            }
            if edge == NO_EDGE {
                break;
            }
            node = self.edges[edge].target;
            if self.terminal[node] {
                keep = length + 1;
            }
        }
        keep
    }
}

impl StreamScrubber {
    /// `None` when there is nothing to look for.
    pub fn new(secrets: &[&str]) -> Option<Self> {
        let secrets = forms(secrets);
        if secrets.is_empty() {
            return None;
        }
        let prefixes = PrefixTrie::new(&secrets);
        Some(Self {
            secrets,
            prefixes,
            carry: Vec::new(),
        })
    }

    /// Scrubs `chunk` and returns what is safe to send. With `last`, nothing is held back.
    pub fn feed(&mut self, chunk: &[u8], last: bool) -> Bytes {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(chunk);
        for secret in &self.secrets {
            blank(&mut data, secret);
        }
        if !last {
            let keep = self.partial_suffix(&data);
            self.carry = data.split_off(data.len() - keep);
        }
        Bytes::from(data)
    }

    /// The longest suffix of `data` that is a proper prefix of some secret.
    fn partial_suffix(&self, data: &[u8]) -> usize {
        self.prefixes.longest_suffix(data)
    }
}

/// Returns `text` with every known form of every secret blanked, for a diagnostic snippet that
/// is not streamed. The replacement keeps the length, as [`StreamScrubber`] does.
pub fn scrub_text(text: &str, secrets: &[&str]) -> String {
    let mut data = text.as_bytes().to_vec();
    for secret in forms(secrets) {
        blank(&mut data, &secret);
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// Whether `text` still contains a key form after all ASCII whitespace and control bytes are
/// removed. This catches a key split in ways the explicit forms did not anticipate, before the
/// printable snippet turns those control bytes into spaces.
pub fn contains_key_material(text: &str, secrets: &[&str]) -> bool {
    let compacted: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && !b.is_ascii_control())
        .collect();
    forms(secrets).iter().any(|form| {
        !form.is_empty()
            && form.len() <= compacted.len()
            && compacted.windows(form.len()).any(|window| window == form)
    })
}

/// Overwrites every occurrence of `secret` in `data` with `[***]` of the same length.
fn blank(data: &mut [u8], secret: &[u8]) {
    let mut i = 0;
    while i + secret.len() <= data.len() {
        if data[i..].starts_with(secret) {
            let end = i + secret.len();
            data[i..end].fill(b'*');
            data[i] = b'[';
            data[end - 1] = b']';
            i = end;
        } else {
            i += 1;
        }
    }
}

/// A body stream with echoed keys scrubbed out of it.
pub struct ScrubbedStream {
    inner: Pin<Box<dyn Stream<Item = ByteResult> + Send>>,
    scrubber: StreamScrubber,
    done: bool,
}

impl ScrubbedStream {
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = ByteResult> + Send>>,
        scrubber: StreamScrubber,
    ) -> Self {
        Self {
            inner,
            scrubber,
            done: false,
        }
    }
}

impl Stream for ScrubbedStream {
    type Item = ByteResult;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => {
                    let out = this.scrubber.feed(&chunk, false);
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(out)));
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    this.done = true;
                    let tail = this.scrubber.feed(&[], true);
                    return Poll::Ready((!tail.is_empty()).then_some(Ok(tail)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-secret-1234";

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut scrubber = StreamScrubber::new(&[KEY, "Bearer sk-secret-1234"]).unwrap();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend_from_slice(&scrubber.feed(chunk, false));
        }
        out.extend_from_slice(&scrubber.feed(&[], true));
        out
    }

    #[test]
    fn a_key_in_one_chunk_is_blanked_with_the_same_length() {
        let body = format!("echo {KEY} end");
        let out = run(&[body.as_bytes()]);
        assert_eq!(out.len(), body.len());
        assert_eq!(String::from_utf8(out).unwrap(), "echo [************] end");
    }

    #[test]
    fn the_formatted_credential_goes_whole() {
        let out = run(&[b"x Bearer sk-secret-1234 y"]);
        assert!(!String::from_utf8(out).unwrap().contains("secret"));
    }

    #[test]
    fn a_key_split_at_every_offset_is_still_caught() {
        let body = format!("data: {{\"echo\":\"{KEY}\"}}\n\n");
        for cut in 0..=body.len() {
            let (a, b) = body.as_bytes().split_at(cut);
            let out = String::from_utf8(run(&[a, b])).unwrap();
            assert!(!out.contains("secret-1234"), "cut {cut}: {out}");
            assert_eq!(out.len(), body.len(), "cut {cut}");
        }
    }

    #[test]
    fn a_key_split_across_three_chunks_is_caught() {
        let out = String::from_utf8(run(&[b"a sk-sec", b"ret-", b"1234 b"])).unwrap();
        assert!(!out.contains("secret"), "{out}");
        assert_eq!(out.len(), "a sk-secret-1234 b".len());
    }

    #[test]
    fn finding_a_partial_suffix_walks_the_chunk_once_not_every_form() {
        let key = format!("sk-{}", "x".repeat(97));
        let scrubber = StreamScrubber::new(&[&key]).unwrap();
        assert!(
            scrubber.secrets.len() > 100,
            "the key must have many forms for this to be meaningful"
        );
        let data = b"prefix sk-xxxx";
        assert!(
            scrubber.partial_suffix(data) > 0,
            "a key prefix is held back"
        );
        assert!(
            scrubber.prefixes.suffix_steps.load(Ordering::Relaxed) <= data.len(),
            "suffix work must follow the chunk length, not the form count"
        );
    }

    #[test]
    fn text_without_a_key_comes_back_unchanged_and_nothing_is_held_back_needlessly() {
        let mut scrubber = StreamScrubber::new(&[KEY]).unwrap();
        let first = scrubber.feed(b"data: hello\n\n", false);
        assert_eq!(
            &first[..],
            b"data: hello\n\n",
            "no partial key at the end, so nothing is held"
        );
        let held = scrubber.feed(b"x sk-sec", false);
        assert_eq!(&held[..], b"x ", "only the possible key start is held");
        assert_eq!(&scrubber.feed(b"ond", true)[..], b"sk-second");
    }

    const ODD: &str = "sk-a/b+c\"d\\e f=1";

    fn json_minimal(c: char) -> String {
        match c {
            '"' => "\\\"".to_owned(),
            '\\' => "\\\\".to_owned(),
            c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
            c => c.to_string(),
        }
    }

    fn aligned(offset: usize, alphabet: &[u8; 64]) -> String {
        let total = offset + ODD.len();
        let start = offset + 1;
        let end = total / 3 * 4
            + match total % 3 {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => unreachable!(),
            };
        let mut data = vec![0; offset];
        data.extend_from_slice(ODD.as_bytes());
        base64(&data, alphabet, false)[start..end].to_string()
    }

    fn encoded_forms() -> Vec<(&'static str, String)> {
        let all_escaped: String = ODD
            .chars()
            .map(|c| format!("\\u{:04x}", c as u32))
            .collect();
        let alnum_escaped: String = ODD
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    format!("\\u{:04x}", c as u32)
                } else {
                    json_minimal(c)
                }
            })
            .collect();
        let one_escaped: String = ODD
            .chars()
            .map(|c| {
                if c == 'a' {
                    "\\u0061".to_owned()
                } else {
                    json_minimal(c)
                }
            })
            .collect();
        let all_percent: String = ODD.bytes().map(|b| format!("%{b:02X}")).collect();
        let unreserved_percent: String = ODD
            .bytes()
            .map(|b| {
                if is_unreserved(b) {
                    format!("%{b:02X}")
                } else {
                    char::from(b).to_string()
                }
            })
            .collect();
        let all_split: String = ODD
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let odd_split = ODD.replacen('c', "c\u{1b}", 1);
        vec![
            ("json minimal", r#"sk-a/b+c\"d\\e f=1"#.to_string()),
            ("json slash", r#"sk-a\/b+c\"d\\e f=1"#.to_string()),
            ("json all escaped", all_escaped),
            ("json alnum escaped", alnum_escaped),
            ("json one escaped", one_escaped),
            ("percent", "sk-a%2Fb%2Bc%22d%5Ce%20f%3D1".to_string()),
            ("percent lower", "sk-a%2fb%2bc%22d%5ce%20f%3d1".to_string()),
            ("percent plus", "sk-a%2Fb%2Bc%22d%5Ce+f%3D1".to_string()),
            ("percent all", all_percent),
            ("percent unreserved", unreserved_percent),
            ("base64", base64(ODD.as_bytes(), B64_STD, true)),
            ("base64 unpadded", base64(ODD.as_bytes(), B64_STD, false)),
            ("base64url", base64(ODD.as_bytes(), B64_URL, true)),
            ("base64url unpadded", base64(ODD.as_bytes(), B64_URL, false)),
            ("base64 offset 1", aligned(1, B64_STD)),
            ("base64 offset 2", aligned(2, B64_STD)),
            ("base64url offset 1", aligned(1, B64_URL)),
            ("base64url offset 2", aligned(2, B64_URL)),
            ("split by space", all_split),
            ("split by control", odd_split),
        ]
    }

    fn run_odd(chunks: &[&[u8]]) -> String {
        let mut scrubber = StreamScrubber::new(&[ODD]).unwrap();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend_from_slice(&scrubber.feed(chunk, false));
        }
        out.extend_from_slice(&scrubber.feed(&[], true));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn base64_encoder_matches_known_vectors() {
        assert_eq!(base64(b"foobar", B64_STD, true), "Zm9vYmFy");
        assert_eq!(base64(b"fooba", B64_STD, true), "Zm9vYmE=");
        assert_eq!(base64(b"foob", B64_STD, true), "Zm9vYg==");
        assert_eq!(base64(b"foob", B64_STD, false), "Zm9vYg");
        assert_eq!(base64(&[0xfb, 0xff], B64_STD, true), "+/8=");
        assert_eq!(base64(&[0xfb, 0xff], B64_URL, true), "-_8=");
    }

    #[test]
    fn every_encoded_form_is_blanked_with_the_same_length() {
        for (name, form) in encoded_forms() {
            let body = format!("a {form} b");
            let out = run_odd(&[body.as_bytes()]);
            assert_eq!(out.len(), body.len(), "{name}");
            assert_eq!(
                out,
                format!("a [{}] b", "*".repeat(form.len() - 2)),
                "{name}"
            );
        }
    }

    #[test]
    fn every_encoded_form_split_at_every_offset_is_caught() {
        for (name, form) in encoded_forms() {
            let body = format!("x\"{form}\"y");
            for cut in 0..=body.len() {
                let (a, b) = body.as_bytes().split_at(cut);
                let out = run_odd(&[a, b]);
                // Cut right after an unpadded form leaves only its `=` padding visible.
                let inner = out
                    .strip_prefix("x\"[")
                    .and_then(|o| o.strip_suffix("\"y"))
                    .map(|o| o.trim_end_matches('=').trim_end_matches(']'))
                    .unwrap_or_else(|| panic!("{name} cut {cut}: {out}"));
                assert!(
                    !inner.is_empty() && inner.bytes().all(|b| b == b'*'),
                    "{name} cut {cut}: {out}"
                );
                assert!(out.len() == body.len(), "{name} cut {cut}: {out}");
            }
        }
    }

    #[test]
    fn an_encoded_form_split_across_three_chunks_is_caught() {
        for (name, form) in encoded_forms() {
            let third = form.len() / 3;
            let (a, rest) = form.as_bytes().split_at(third);
            let (b, c) = rest.split_at(third);
            let out = run_odd(&[a, b, c]);
            assert_eq!(out, format!("[{}]", "*".repeat(form.len() - 2)), "{name}");
        }
    }

    #[test]
    fn forms_are_ordered_longest_first_and_unique() {
        let all = forms(&[ODD, "Bearer sk-a/b+c\"d\\e f=1"]);
        assert!(all.windows(2).all(|w| w[0].len() >= w[1].len()));
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
        assert!(all.iter().all(|f| f.len() >= MIN_SECRET));
    }

    #[test]
    fn a_longer_form_of_a_later_key_is_blanked_before_a_shorter_one() {
        let short = "sk-12345678";
        let long = format!("Bearer {short}-long");
        let all = forms(&[short, &long]);
        let short_at = all
            .iter()
            .position(|f| f.as_slice() == short.as_bytes())
            .unwrap();
        let long_at = all
            .iter()
            .position(|f| f.as_slice() == long.as_bytes())
            .unwrap();
        assert!(
            long_at < short_at,
            "the longer form must come first across secrets"
        );
        let text = format!("echo {long}");
        let out = scrub_text(&text, &[short, &long]);
        assert!(!out.contains("-long"), "{out}");
        assert_eq!(out.len(), text.len());
    }

    #[test]
    fn a_long_key_still_gets_a_base64_form() {
        let key = format!("sk-{}", "a".repeat(597));
        let forms = forms(&[&key]);
        let encoded = base64(key.as_bytes(), B64_STD, true);
        assert!(
            forms
                .iter()
                .any(|form| form.as_slice() == encoded.as_bytes()),
            "the plain base64 form must survive the budget"
        );
        assert!(forms.iter().any(|form| form.as_slice() == key.as_bytes()));
    }

    #[test]
    fn a_maximum_length_key_has_a_bounded_number_and_size_of_forms() {
        let key = "x".repeat(MAX_SECRET);
        let forms = forms(&[&key]);
        assert!(forms.len() <= MAX_FORMS_PER_SECRET);
        let bytes: usize = forms.iter().map(Vec::len).sum();
        assert!(bytes < 64 * 1024 * 1024, "forms used {bytes} bytes");
        assert!(forms.windows(2).all(|w| w[0].len() >= w[1].len()));
    }

    #[test]
    fn scrub_text_blanks_every_form_and_leaves_length_alone() {
        let text = format!("raw {KEY} json {ODD}");
        let out = scrub_text(&text, &[KEY, ODD]);
        assert_eq!(out.len(), text.len());
        assert!(!out.contains("secret"), "{out}");
        assert!(!out.contains("sk-"), "{out}");
        assert_eq!(scrub_text("nothing to hide", &[KEY]), "nothing to hide");
    }

    #[test]
    fn ordinary_text_is_untouched_by_the_encoded_forms() {
        let text = "data: {\"id\":\"chatcmpl-1\",\"url\":\"https://example.com/a%20b?x=1+2\",\"b64\":\"SGVsbG8gd29ybGQ=\",\"path\":\"sk-a/b\"}\n\n";
        assert_eq!(run_odd(&[text.as_bytes()]), text);
        let mut scrubber = StreamScrubber::new(&[ODD]).unwrap();
        let out = scrubber.feed(text.as_bytes(), false);
        assert_eq!(&out[..], text.as_bytes());
    }

    #[test]
    fn forms_are_capped_per_secret() {
        let long = "k".repeat(512);
        assert!(forms(&[&long]).len() <= MAX_FORMS_PER_SECRET);
    }

    #[test]
    fn a_long_key_with_an_inserted_space_is_scrubbed() {
        let key = format!("sk-{}", "a".repeat(97));
        assert_eq!(key.len(), 100);
        let mut scrubber = StreamScrubber::new(&[&key]).unwrap();
        for split in 1..key.len() {
            let echoed = format!("echo {} {} end", &key[..split], &key[split..]);
            let expected = format!("echo [{}] end", "*".repeat(key.len() - 1));
            let out = scrubber.feed(echoed.as_bytes(), true);
            assert_eq!(out.len(), echoed.len(), "split {split}");
            assert_eq!(
                String::from_utf8(out.to_vec()).unwrap(),
                expected,
                "split {split}"
            );
        }
        let echoed = format!("echo {} {} end", &key[..50], &key[50..]);
        assert!(!scrub_text(&echoed, &[&key]).contains(&key[..20]));
    }

    #[test]
    fn compacted_text_with_key_material_is_detected() {
        let doubled = format!("{}  {}", &KEY[..5], &KEY[5..]);
        assert!(!scrub_text(&doubled, &[KEY]).contains(KEY));
        assert!(contains_key_material(&doubled, &[KEY]));
        let once = format!("{} {}", &KEY[..5], &KEY[5..]);
        let cleaned = scrub_text(&once, &[KEY]);
        assert!(!cleaned.contains(KEY));
        assert!(!contains_key_material(&cleaned, &[KEY]));
        assert!(!contains_key_material("nothing to hide", &[KEY]));
    }

    #[test]
    fn short_secrets_are_ignored() {
        assert!(StreamScrubber::new(&["short", ""]).is_none());
    }
}
