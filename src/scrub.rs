//! Removes an upstream key that a successful response echoes back, as the body streams.
//!
//! A key can arrive split across two chunks, so the scrubber keeps back only the tail that could
//! still become the start of a key. That is usually nothing, so a server-sent event is not held
//! up. The replacement has the length of what it replaces, so a response with a content length
//! keeps a correct one.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Bytes;
use futures_core::Stream;

/// A secret shorter than this is more likely ordinary text than a key.
const MIN_SECRET: usize = 8;

type ByteResult = Result<Bytes, std::io::Error>;

/// Every byte string that stands for one of `secrets`, longest first.
pub fn forms(secrets: &[&str]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for secret in secrets.iter().filter(|s| s.len() >= MIN_SECRET) {
        let raw = secret.as_bytes();
        out.push(raw.to_vec());
        out.extend(json_forms(secret));
        out.extend(percent_forms(raw));
        for alphabet in [B64_STD, B64_URL] {
            for padded in [true, false] {
                let encoded = base64(raw, alphabet, padded);
                out.extend(json_forms(&encoded));
                out.extend(percent_forms(encoded.as_bytes()));
                out.push(encoded.into_bytes());
            }
        }
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

/// Minimal escapes, with `\/`, and with every non-alphanumeric as `\uXXXX` in either hex case.
fn json_forms(text: &str) -> Vec<Vec<u8>> {
    let mut forms = Vec::new();
    for style in 0..4 {
        let mut s = String::new();
        for c in text.chars() {
            if style >= 2 && !c.is_ascii_alphanumeric() {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    if style == 2 {
                        s.push_str(&format!("\\u{unit:04x}"));
                    } else {
                        s.push_str(&format!("\\u{unit:04X}"));
                    }
                }
            } else {
                match c {
                    '"' => s.push_str("\\\""),
                    '\\' => s.push_str("\\\\"),
                    '/' if style == 1 => s.push_str("\\/"),
                    c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
                    c => s.push(c),
                }
            }
        }
        if s != text {
            forms.push(s.into_bytes());
        }
    }
    forms
}

/// Lower and upper case hex, and a space as `+`.
fn percent_forms(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut forms = Vec::new();
    for (upper, plus) in [(true, false), (false, false), (true, true), (false, true)] {
        let hex: &[u8; 16] = if upper {
            b"0123456789ABCDEF"
        } else {
            b"0123456789abcdef"
        };
        let mut out = Vec::with_capacity(raw.len());
        for &b in raw {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                out.push(b);
            } else if plus && b == b' ' {
                out.push(b'+');
            } else {
                out.extend_from_slice(&[b'%', hex[usize::from(b >> 4)], hex[usize::from(b & 15)]]);
            }
        }
        if out != raw {
            forms.push(out);
        }
    }
    forms
}

pub struct StreamScrubber {
    secrets: Vec<Vec<u8>>,
    carry: Vec<u8>,
}

impl StreamScrubber {
    /// `None` when there is nothing to look for.
    pub fn new(secrets: &[&str]) -> Option<Self> {
        let secrets = forms(secrets);
        if secrets.is_empty() {
            return None;
        }
        Some(Self {
            secrets,
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
        let mut keep = 0;
        for secret in &self.secrets {
            let most = (secret.len() - 1).min(data.len());
            for len in (keep + 1..=most).rev() {
                if data.ends_with(&secret[..len]) {
                    keep = len;
                    break;
                }
            }
        }
        keep
    }
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

    fn encoded_forms() -> Vec<(&'static str, String)> {
        vec![
            ("json minimal", r#"sk-a/b+c\"d\\e f=1"#.to_string()),
            ("json slash", r#"sk-a\/b+c\"d\\e f=1"#.to_string()),
            ("json unicode", r#"sk-a/b+c"d\e f=1"#.to_string()),
            ("percent", "sk-a%2Fb%2Bc%22d%5Ce%20f%3D1".to_string()),
            ("percent lower", "sk-a%2fb%2bc%22d%5ce%20f%3d1".to_string()),
            ("percent plus", "sk-a%2Fb%2Bc%22d%5Ce+f%3D1".to_string()),
            ("base64", base64(ODD.as_bytes(), B64_STD, true)),
            ("base64 unpadded", base64(ODD.as_bytes(), B64_STD, false)),
            ("base64url", base64(ODD.as_bytes(), B64_URL, true)),
            ("base64url unpadded", base64(ODD.as_bytes(), B64_URL, false)),
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
    fn ordinary_text_is_untouched_by_the_encoded_forms() {
        let text = "data: {\"id\":\"chatcmpl-1\",\"url\":\"https://example.com/a%20b?x=1+2\",\"b64\":\"SGVsbG8gd29ybGQ=\",\"path\":\"sk-a/b\"}\n\n";
        assert_eq!(run_odd(&[text.as_bytes()]), text);
        let mut scrubber = StreamScrubber::new(&[ODD]).unwrap();
        let out = scrubber.feed(text.as_bytes(), false);
        assert_eq!(&out[..], text.as_bytes());
    }

    #[test]
    fn short_secrets_are_ignored() {
        assert!(StreamScrubber::new(&["short", ""]).is_none());
    }
}
