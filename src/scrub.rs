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

pub struct StreamScrubber {
    secrets: Vec<Vec<u8>>,
    carry: Vec<u8>,
}

impl StreamScrubber {
    /// `None` when there is nothing to look for.
    pub fn new(secrets: &[&str]) -> Option<Self> {
        let mut secrets: Vec<Vec<u8>> = secrets
            .iter()
            .filter(|s| s.len() >= MIN_SECRET)
            .map(|s| s.as_bytes().to_vec())
            .collect();
        if secrets.is_empty() {
            return None;
        }
        // The formatted credential contains the raw key, so the longer one goes first.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
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

    #[test]
    fn short_secrets_are_ignored() {
        assert!(StreamScrubber::new(&["short", ""]).is_none());
    }
}
