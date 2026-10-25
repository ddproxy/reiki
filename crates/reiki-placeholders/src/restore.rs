//! Puts original values back into responses, including event streams that split a placeholder.

use std::collections::HashMap;

use bytes::Bytes;
use serde_json::Value;

use crate::mask::{HEAD, TAIL};

/// Longer than any placeholder this plugin writes, so a stray `[[REIKI_` is not held forever.
const MAX_PLACEHOLDER: usize = 48;

/// Replaces every complete placeholder that was written for this request.
pub fn restore_text(text: &str, reverse: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(HEAD) {
        out.push_str(&rest[..at]);
        let candidate = &rest[at..];
        match candidate.find(TAIL) {
            Some(end) if end + TAIL.len() <= MAX_PLACEHOLDER => {
                let token = &candidate[..end + TAIL.len()];
                out.push_str(reverse.get(token).map_or(token, String::as_str));
                rest = &candidate[token.len()..];
            }
            _ => {
                out.push_str(HEAD);
                rest = &candidate[HEAD.len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where a placeholder that has only partly arrived begins, or the text length when none does.
pub fn incomplete_start(text: &str) -> usize {
    let Some(at) = text.rfind("[[") else {
        return text.len();
    };
    let tail = &text[at..];
    if tail.len() > MAX_PLACEHOLDER || tail.contains(TAIL) || !looks_partial(tail) {
        return text.len();
    }
    at
}

fn looks_partial(tail: &str) -> bool {
    // One trailing bracket may be the first half of the closing pair.
    let body = tail.strip_suffix(']').unwrap_or(tail);
    if body.len() <= HEAD.len() {
        return HEAD.starts_with(body);
    }
    body.starts_with(HEAD)
        && body[HEAD.len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Stream,
    Whole,
}

/// Restores one response as its chunks arrive.
pub struct Restorer {
    mode: Mode,
    buf: Vec<u8>,
    /// Partial placeholders held back per JSON field until the next event completes them.
    held: HashMap<String, String>,
    cap: usize,
    given_up: bool,
}

impl Restorer {
    pub fn new(cap: usize) -> Self {
        Self {
            mode: Mode::Idle,
            buf: Vec::new(),
            held: HashMap::new(),
            cap,
            given_up: false,
        }
    }

    /// Chooses how to read the body from its headers; an encoded body is left alone.
    pub fn start(&mut self, content_type: Option<&str>, encoded: bool) {
        self.mode = if encoded {
            Mode::Idle
        } else if content_type.is_some_and(|t| t.to_ascii_lowercase().contains("text/event-stream"))
        {
            Mode::Stream
        } else {
            Mode::Whole
        };
    }

    pub fn feed(&mut self, chunk: &mut Bytes, is_last: bool, reverse: &HashMap<String, String>) {
        match self.mode {
            Mode::Idle => {}
            Mode::Whole => self.feed_whole(chunk, is_last, reverse),
            Mode::Stream => self.feed_stream(chunk, is_last, reverse),
        }
    }

    fn feed_whole(&mut self, chunk: &mut Bytes, is_last: bool, reverse: &HashMap<String, String>) {
        if self.given_up {
            return;
        }
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > self.cap {
            self.given_up = true;
            *chunk = Bytes::from(std::mem::take(&mut self.buf));
            return;
        }
        if !is_last {
            *chunk = Bytes::new();
            return;
        }
        let all = std::mem::take(&mut self.buf);
        *chunk = match String::from_utf8(all) {
            Ok(text) => Bytes::from(restore_text(&text, reverse)),
            Err(error) => Bytes::from(error.into_bytes()),
        };
    }

    fn feed_stream(&mut self, chunk: &mut Bytes, is_last: bool, reverse: &HashMap<String, String>) {
        if self.given_up {
            return;
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(len) = event_len(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..len).collect();
            out.extend(self.event(&event, reverse));
        }
        if self.buf.len() > self.cap {
            // No event boundary within the cap: stop buffering and pass the rest through.
            self.given_up = true;
            out.extend(std::mem::take(&mut self.buf));
        }
        if is_last && !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            out.extend(self.event(&rest, reverse));
        }
        if is_last && !self.held.is_empty() {
            tracing::debug!(
                fields = self.held.len(),
                "placeholders: stream ended mid-placeholder"
            );
        }
        *chunk = Bytes::from(out);
    }

    fn event(&mut self, event: &[u8], reverse: &HashMap<String, String>) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(event) else {
            return event.to_vec();
        };
        // Skipping events with no `[[` keeps most events byte-exact.
        if self.held.is_empty() && !text.contains("[[") {
            return event.to_vec();
        }
        let mut out = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            let eol = &line[body.len()..];
            match body.strip_prefix("data:") {
                Some(rest) => {
                    let payload = rest.strip_prefix(' ').unwrap_or(rest);
                    out.push_str("data: ");
                    out.push_str(&self.payload(payload, reverse));
                    out.push_str(eol);
                }
                None => out.push_str(line),
            }
        }
        out.into_bytes()
    }

    fn payload(&mut self, payload: &str, reverse: &HashMap<String, String>) -> String {
        if payload.trim() == "[DONE]" {
            return payload.to_owned();
        }
        match serde_json::from_str::<Value>(payload) {
            Ok(mut value) => {
                self.walk("", &mut value, reverse);
                value.to_string()
            }
            Err(_) => self.text_at("data", payload, reverse),
        }
    }

    fn walk(&mut self, path: &str, value: &mut Value, reverse: &HashMap<String, String>) {
        match value {
            Value::String(text) => *text = self.text_at(path, text, reverse),
            Value::Array(items) => {
                for (i, item) in items.iter_mut().enumerate() {
                    self.walk(&format!("{path}[{i}]"), item, reverse);
                }
            }
            Value::Object(map) => {
                for (key, item) in map.iter_mut() {
                    self.walk(&format!("{path}.{key}"), item, reverse);
                }
            }
            _ => {}
        }
    }

    /// Restores `text` after any tail held for this field, holding back a new partial placeholder.
    fn text_at(&mut self, path: &str, text: &str, reverse: &HashMap<String, String>) -> String {
        let mut combined = self.held.remove(path).unwrap_or_default();
        combined.push_str(text);
        let restored = restore_text(&combined, reverse);
        let keep = incomplete_start(&restored);
        if keep == restored.len() {
            return restored;
        }
        self.held
            .insert(path.to_owned(), restored[keep..].to_owned());
        restored[..keep].to_owned()
    }
}

/// Length of the first complete event, delimiter included.
fn event_len(buf: &[u8]) -> Option<usize> {
    let unix = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let dos = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    match (unix, dos) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALCON: &str = "[[REIKI_TERM_aaaaaaaa]]";

    fn reverse() -> HashMap<String, String> {
        HashMap::from([(FALCON.to_owned(), "Falcon".to_owned())])
    }

    fn event(content: &str) -> String {
        let json = serde_json::json!({"choices": [{"delta": {"content": content}}]});
        format!("data: {json}\n\n")
    }

    /// The text of every `delta.content` in an event stream, in order.
    fn contents(stream: &[u8]) -> String {
        String::from_utf8_lossy(stream)
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|p| serde_json::from_str::<Value>(p).ok())
            .filter_map(|v| {
                v["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect()
    }

    fn run_stream(chunks: &[&[u8]]) -> Vec<u8> {
        let mut restorer = Restorer::new(1024);
        restorer.start(Some("text/event-stream; charset=utf-8"), false);
        let reverse = reverse();
        let mut out = Vec::new();
        for (i, part) in chunks.iter().enumerate() {
            let mut chunk = Bytes::copy_from_slice(part);
            restorer.feed(&mut chunk, i + 1 == chunks.len(), &reverse);
            out.extend_from_slice(&chunk);
        }
        out
    }

    #[test]
    fn complete_placeholders_are_restored_and_unknown_ones_left_alone() {
        let text = format!("a {FALCON} b [[REIKI_TERM_bbbbbbbb]] c [[REIKI_ d");
        assert_eq!(
            restore_text(&text, &reverse()),
            "a Falcon b [[REIKI_TERM_bbbbbbbb]] c [[REIKI_ d"
        );
        assert_eq!(
            restore_text(&format!("{FALCON}{FALCON}"), &reverse()),
            "FalconFalcon"
        );
    }

    #[test]
    fn a_partly_arrived_placeholder_is_recognised_at_the_end_of_text() {
        assert_eq!(incomplete_start("plain text"), 10);
        assert_eq!(incomplete_start("see [["), 4);
        assert_eq!(incomplete_start("see [[REI"), 4);
        assert_eq!(incomplete_start("see [[REIKI_TERM_aa"), 4);
        assert_eq!(incomplete_start("see [[REIKI_TERM_aaaaaaaa]"), 4);
        // Finished, unrelated or too long: nothing to hold.
        assert_eq!(incomplete_start("see [[REIKI_TERM_aaaaaaaa]]"), 27);
        assert_eq!(incomplete_start("list [1] and ["), 14);
        assert_eq!(incomplete_start("json [[1, 2"), 11);
        let long = format!("x [[REIKI_{}", "A".repeat(60));
        assert_eq!(incomplete_start(&long), long.len());
    }

    #[test]
    fn a_placeholder_split_across_events_is_restored_whole() {
        let e1 = event("The [[REI");
        let e2 = event("KI_TERM_aaaaaaaa]] plan");
        let done = "data: [DONE]\n\n";
        let out = run_stream(&[e1.as_bytes(), e2.as_bytes(), done.as_bytes()]);
        assert_eq!(contents(&out), "The Falcon plan");
        assert!(String::from_utf8_lossy(&out).ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn the_result_does_not_depend_on_where_the_network_splits_the_stream() {
        let whole = [
            event("go [[REIKI_TERM_"),
            event("aaaaaaaa]] now"),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat();
        let expected = contents(&run_stream(&[whole.as_bytes()]));
        assert_eq!(expected, "go Falcon now");
        for size in [1, 2, 3, 5, 7, 11, 64] {
            let chunks: Vec<&[u8]> = whole.as_bytes().chunks(size).collect();
            assert_eq!(
                contents(&run_stream(&chunks)),
                expected,
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn events_without_placeholders_keep_their_exact_bytes() {
        let plain = "event: ping\ndata: {\"b\": 1,   \"a\": 2}\n\n";
        let out = run_stream(&[plain.as_bytes(), b"data: [DONE]\n\n"]);
        assert_eq!(out, [plain, "data: [DONE]\n\n"].concat().into_bytes());
    }

    #[test]
    fn crlf_delimited_events_work_too() {
        let e =
            format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{FALCON}\"}}}}]}}\r\n\r\n");
        assert_eq!(contents(&run_stream(&[e.as_bytes()])), "Falcon");
    }

    #[test]
    fn a_body_that_is_not_a_stream_is_restored_when_it_ends() {
        let mut restorer = Restorer::new(1024);
        restorer.start(Some("application/json"), false);
        let reverse = reverse();
        let body = format!("{{\"text\":\"{FALCON} flies\"}}");
        let (a, b) = body.as_bytes().split_at(9);
        let (mut one, mut two, mut three) = (
            Bytes::copy_from_slice(a),
            Bytes::copy_from_slice(b),
            Bytes::new(),
        );
        restorer.feed(&mut one, false, &reverse);
        restorer.feed(&mut two, false, &reverse);
        assert!(one.is_empty() && two.is_empty(), "held until the end");
        restorer.feed(&mut three, true, &reverse);
        assert_eq!(&three[..], b"{\"text\":\"Falcon flies\"}");
    }

    #[test]
    fn a_body_over_the_cap_is_released_unchanged() {
        let mut restorer = Restorer::new(8);
        restorer.start(Some("application/json"), false);
        let mut chunk = Bytes::from(format!("{FALCON} is long"));
        restorer.feed(&mut chunk, false, &reverse());
        assert_eq!(&chunk[..], format!("{FALCON} is long").as_bytes());
        let mut tail = Bytes::from_static(b" more");
        restorer.feed(&mut tail, true, &reverse());
        assert_eq!(&tail[..], b" more");
    }

    #[test]
    fn a_stream_with_no_event_boundary_stops_buffering_at_the_cap() {
        let mut restorer = Restorer::new(16);
        restorer.start(Some("text/event-stream"), false);
        let reverse = reverse();
        let mut first = Bytes::from_static(b"data: xxxxxxxxxxxxxxxxxxxxxxxx");
        restorer.feed(&mut first, false, &reverse);
        assert_eq!(
            &first[..],
            b"data: xxxxxxxxxxxxxxxxxxxxxxxx",
            "released unchanged"
        );
        let mut later = Bytes::from_static(b"more");
        restorer.feed(&mut later, true, &reverse);
        assert_eq!(&later[..], b"more");
    }

    #[test]
    fn an_encoded_body_is_never_touched() {
        let mut restorer = Restorer::new(1024);
        restorer.start(Some("application/json"), true);
        let mut chunk = Bytes::from(FALCON.to_owned());
        restorer.feed(&mut chunk, true, &reverse());
        assert_eq!(&chunk[..], FALCON.as_bytes());
    }
}
