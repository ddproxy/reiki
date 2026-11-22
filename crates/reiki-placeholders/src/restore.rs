//! Puts original values back into responses, including event streams that split a placeholder.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Map, Value};

use crate::mask::{HEAD, TAIL};

/// Longer than any placeholder this plugin writes, so a stray `[[REIKI_` is not held forever.
const MAX_PLACEHOLDER: usize = 48;

/// Prefix of the placeholders that stand for credentials rather than configured terms.
const SECRET_HEAD: &str = "[[REIKI_SECRET_";

/// Field names under which a model's tool call carries its arguments.
const TOOL_CALL_MARKERS: &[&str] = &[
    "tool_call",
    "function_call",
    "functionCall",
    "tool_use",
    "toolUse",
    "partial_json",
];

/// Replaces every complete placeholder that was written for this request.
pub fn restore_text(text: &str, reverse: &HashMap<String, String>) -> String {
    restore(text, reverse, true)
}

/// Like `restore_text`, but credential placeholders stay masked unless `secrets` is set.
fn restore(text: &str, reverse: &HashMap<String, String>, secrets: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(HEAD) {
        out.push_str(&rest[..at]);
        let candidate = &rest[at..];
        match candidate.find(TAIL) {
            Some(end) if end + TAIL.len() <= MAX_PLACEHOLDER => {
                let token = &candidate[..end + TAIL.len()];
                let keep = !secrets && token.starts_with(SECRET_HEAD);
                out.push_str(match reverse.get(token) {
                    Some(original) if !keep => original,
                    _ => token,
                });
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
    // A placeholder is at most MAX_PLACEHOLDER bytes, so only the last window can hold an unfinished one.
    let from = text.len().saturating_sub(MAX_PLACEHOLDER);
    (from..text.len())
        .find(|&at| {
            text.as_bytes()[at] == b'[' && {
                let tail = &text[at..];
                !tail.contains(TAIL) && looks_partial(tail)
            }
        })
        .unwrap_or(text.len())
}

/// True when `text` could end, or contain, the start of a placeholder: a `[[` pair, or a lone `[`
/// that closes a JSON string (`["`), a raw line or the text. Other events keep their exact bytes.
fn may_hold(text: &str) -> bool {
    text.contains("[[")
        || text.match_indices('[').any(|(at, _)| {
            matches!(
                text.as_bytes().get(at + 1),
                None | Some(b'"' | b'\n' | b'\r')
            )
        })
}

/// The smallest JSON value that carries `text` at `at`, shaped like the stream's own events.
fn field_event(at: &[Seg], text: &str) -> Value {
    at.iter()
        .rev()
        .fold(Value::String(text.to_owned()), |inner, seg| match seg {
            Seg::Key(key) => {
                let mut map = Map::new();
                map.insert(key.clone(), inner);
                Value::Object(map)
            }
            Seg::Index(index) => {
                let mut items = vec![Value::Null; *index];
                items.push(inner);
                Value::Array(items)
            }
            Seg::Raw => inner,
        })
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

/// True for the terminal event of an Anthropic stream.
fn ends_message(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|kind| kind == "message_stop")
}

/// True when any data line of the event is the terminal `message_stop` payload.
fn event_ends_message(event: &str) -> bool {
    event.lines().any(|line| {
        line.strip_prefix("data:")
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
            .is_some_and(ends_message)
    })
}

/// The content block index an Anthropic `content_block_stop` data line names, if any.
fn block_stop_index(event: &str) -> Option<usize> {
    event.lines().find_map(|line| {
        line.strip_prefix("data:")
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
            .and_then(|payload| {
                let value = serde_json::from_str::<Value>(payload).ok()?;
                (value["type"].as_str() == Some("content_block_stop"))
                    .then(|| value["index"].as_u64().map(|i| i as usize))?
            })
    })
}

/// The payload's `type` and `index` — the event kind when no `event:` line names one.
fn payload_kind_index(payload: &str) -> (String, Option<usize>) {
    match serde_json::from_str::<Value>(payload) {
        Ok(value) => (
            value["type"].as_str().unwrap_or_default().to_owned(),
            value["index"].as_u64().map(|i| i as usize),
        ),
        Err(_) => (String::new(), None),
    }
}

/// Where a held tail belongs: a JSON key or array index, or the text of a payload that is not JSON.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Seg {
    Key(String),
    Index(usize),
    Raw,
}

/// What identifies a held tail: the event kind, the content block index when the event has one, and
/// the field path. The block index is what keeps a tail held in one block out of the next.
type HeldKey = (String, Option<usize>, Vec<Seg>);

/// The envelope of the data line that held a tail, so its flush is a complete event.
#[derive(Clone)]
struct Frame {
    /// The `data:` payload as it arrived, replayed with the held text in place of the tail.
    raw: Arc<str>,
    /// Event lines before the data line, such as `event: content_block_delta`.
    pre: Arc<str>,
    /// The data line's own line ending, CRLF included.
    eol: Arc<str>,
    /// Lines after the data line, including the blank line that ends the event.
    post: Arc<str>,
    /// The event kind: the SSE `event:` name, else the payload's `type`, else empty.
    kind: String,
    /// The content block index the event names, when it has one.
    index: Option<usize>,
}

/// A held tail plus the framing of the event that held it.
struct HeldData {
    text: String,
    frame: Frame,
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
    /// Partial placeholders held back per event kind, content block and field until the next event
    /// completes them.
    held: HashMap<HeldKey, HeldData>,
    cap: usize,
    given_up: bool,
    /// Whether credentials are restored inside tool-call arguments too.
    tool_args: bool,
}

/// True for an object that declares itself a tool call by value (`"type": "tool_use"`), as
/// Anthropic and OpenAI Responses do, so its fields are tool arguments whatever they are named.
fn declares_tool_call(map: &serde_json::Map<String, Value>) -> bool {
    map.get("type").and_then(Value::as_str).is_some_and(|t| {
        ["tool_use", "tool_call", "function_call"]
            .iter()
            .any(|m| t.contains(m))
    })
}

/// The fields of an object whose bytes must never be rewritten: signed reasoning and base64
/// media. Anything else in those objects, a text document included, is ordinary content.
pub(crate) fn protected_keys(map: &serde_json::Map<String, Value>) -> &'static [&'static str] {
    let kind = map.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "thinking" => &["thinking", "signature"],
        "redacted_thinking" => &["data"],
        "thinking_delta" => &["thinking"],
        "signature_delta" => &["signature"],
        "image" | "document" => {
            let base64 = map
                .get("source")
                .and_then(|source| source.get("type"))
                .and_then(Value::as_str)
                == Some("base64");
            if base64 { &["source"] } else { &[] }
        }
        _ => &[],
    }
}

/// Paths of every field in `value` that must stay byte-identical.
pub(crate) fn opaque_paths(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                opaque_paths(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for key in protected_keys(map) {
                out.push(format!("{path}.{key}"));
            }
            for (key, item) in map {
                opaque_paths(item, &format!("{path}.{key}"), out);
            }
        }
        _ => {}
    }
}

/// True when `path` names a field below `prefix`.
pub(crate) fn within(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with(['.', '[']))
}

/// Paths of every object in `value` that declares itself a tool call.
fn tool_call_paths(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                tool_call_paths(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            if declares_tool_call(map) {
                out.push(path.to_owned());
            }
            for (key, item) in map {
                tool_call_paths(item, &format!("{path}.{key}"), out);
            }
        }
        _ => {}
    }
}

/// True when `path` (or, for a whole body, the text) sits inside a tool call.
fn in_tool_call(path: &str) -> bool {
    TOOL_CALL_MARKERS.iter().any(|m| path.contains(m))
}

impl Restorer {
    pub fn new(cap: usize, tool_args: bool) -> Self {
        Self {
            tool_args,
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
            Ok(text) => Bytes::from(self.restore_whole(&text, reverse)),
            Err(error) => Bytes::from(error.into_bytes()),
        };
    }

    /// A credential restored into a tool call would be run or echoed by the client, so a body that
    /// carries tool calls is walked as JSON and those fields keep their placeholders.
    fn restore_whole(&mut self, text: &str, reverse: &HashMap<String, String>) -> String {
        let Ok(parsed) = serde_json::from_str::<Value>(text) else {
            return restore(text, reverse, self.tool_args);
        };
        let mut tool_paths = Vec::new();
        tool_call_paths(&parsed, "", &mut tool_paths);
        let mut opaque = Vec::new();
        opaque_paths(&parsed, "", &mut opaque);
        // Nothing needs the structural walk, so keep every byte outside string values too.
        if opaque.is_empty()
            && (self.tool_args || !TOOL_CALL_MARKERS.iter().any(|m| text.contains(m)))
        {
            return restore_text(text, reverse);
        }
        let in_call = |path: &str| {
            in_tool_call(path)
                || tool_paths
                    .iter()
                    .any(|prefix| within(path, prefix) || path == prefix)
        };
        let keep = |path: &str| {
            opaque
                .iter()
                .any(|prefix| within(path, prefix) || path == prefix)
        };
        let mut spans = Vec::new();
        let mut at = 0;
        if collect_strings(text, &mut at, "", &mut spans, 0).is_none() {
            return restore(text, reverse, self.tool_args);
        }
        // Only the strings that change are rewritten, so key order and layout survive.
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for (start, end, path) in spans {
            if keep(&path) {
                continue;
            }
            let Ok(decoded) = serde_json::from_str::<String>(&text[start..end]) else {
                continue;
            };
            let restored = restore(&decoded, reverse, self.tool_args || !in_call(&path));
            if restored != decoded {
                out.push_str(&text[last..start]);
                out.push_str(&serde_json::to_string(&restored).unwrap_or_default());
                last = end;
            }
        }
        out.push_str(&text[last..]);
        out
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
            out.extend(self.flush().into_bytes());
            out.extend(std::mem::take(&mut self.buf));
        }
        if is_last && !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            out.extend(self.event(&rest, reverse));
        }
        if is_last {
            if !self.held.is_empty() {
                tracing::debug!(
                    fields = self.held.len(),
                    "placeholders: stream ended mid-placeholder"
                );
            }
            out.extend(self.flush().into_bytes());
        }
        *chunk = Bytes::from(out);
    }

    fn event(&mut self, event: &[u8], reverse: &HashMap<String, String>) -> Vec<u8> {
        let Ok(text) = std::str::from_utf8(event) else {
            return event.to_vec();
        };
        if self.held.is_empty() && !may_hold(text) {
            return event.to_vec();
        }
        let mut out = String::with_capacity(text.len());
        if event_ends_message(text) {
            out.push_str(&self.flush());
        }
        if let Some(index) = block_stop_index(text) {
            // The block's tails go out as complete deltas before the stop event.
            out.push_str(&self.flush_block(index));
        }
        let mut before = String::new();
        let mut name: Option<Arc<str>> = None;
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            let eol = &line[body.len()..];
            let after = before.len() + line.len();
            match body.strip_prefix("data:") {
                Some(rest) => {
                    let payload = rest.strip_prefix(' ').unwrap_or(rest);
                    let (kind, index) = payload_kind_index(payload);
                    if payload.trim() == "[DONE]" {
                        out.push_str(&self.flush());
                    }
                    let frame = Frame {
                        raw: Arc::from(payload),
                        pre: Arc::from(before.as_str()),
                        eol: Arc::from(eol),
                        post: Arc::from(&text[after..]),
                        kind: name.as_deref().map(str::to_owned).unwrap_or(kind),
                        index,
                    };
                    out.push_str("data: ");
                    out.push_str(&self.payload(payload, reverse, &frame));
                    out.push_str(eol);
                }
                None => {
                    if let Some(event_name) = body.strip_prefix("event:") {
                        name = Some(Arc::from(event_name.trim()));
                    }
                    out.push_str(line);
                }
            }
            before.push_str(line);
        }
        out.into_bytes()
    }

    fn payload(
        &mut self,
        payload: &str,
        reverse: &HashMap<String, String>,
        frame: &Frame,
    ) -> String {
        if payload.trim() == "[DONE]" {
            return payload.to_owned();
        }
        match serde_json::from_str::<Value>(payload) {
            Ok(mut value) => {
                self.walk(&mut Vec::new(), false, false, &mut value, reverse, frame);
                value.to_string()
            }
            Err(_) => self.text_at(&[Seg::Raw], false, payload, reverse, frame),
        }
    }

    /// Restores every string in `value`; `at` is where it sits, `in_call` whether that is inside a
    /// tool call and `opaque` whether the bytes must stay untouched.
    fn walk(
        &mut self,
        at: &mut Vec<Seg>,
        in_call: bool,
        opaque: bool,
        value: &mut Value,
        reverse: &HashMap<String, String>,
        frame: &Frame,
    ) {
        match value {
            Value::String(text) => {
                if !opaque {
                    *text = self.text_at(at, in_call, text, reverse, frame);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter_mut().enumerate() {
                    at.push(Seg::Index(i));
                    self.walk(at, in_call, opaque, item, reverse, frame);
                    at.pop();
                }
            }
            Value::Object(map) => {
                let protected = protected_keys(map);
                let in_call = in_call || declares_tool_call(map);
                for (key, item) in map.iter_mut() {
                    at.push(Seg::Key(key.clone()));
                    let opaque = opaque || protected.contains(&key.as_str());
                    self.walk(
                        at,
                        in_call || in_tool_call(key),
                        opaque,
                        item,
                        reverse,
                        frame,
                    );
                    at.pop();
                }
            }
            _ => {}
        }
    }

    /// Restores `text` after any tail held for this field, holding back a new partial placeholder.
    fn text_at(
        &mut self,
        at: &[Seg],
        in_call: bool,
        text: &str,
        reverse: &HashMap<String, String>,
        frame: &Frame,
    ) -> String {
        let key = (frame.kind.clone(), frame.index, at.to_vec());
        let mut combined = self
            .held
            .remove(&key)
            .map(|held| held.text)
            .unwrap_or_default();
        combined.push_str(text);
        let restored = restore(&combined, reverse, self.tool_args || !in_call);
        let keep = incomplete_start(&restored);
        if keep == restored.len() {
            return restored;
        }
        self.held.insert(
            key,
            HeldData {
                text: restored[keep..].to_owned(),
                frame: frame.clone(),
            },
        );
        restored[..keep].to_owned()
    }

    /// Every held tail as its own complete event, so a stream that ends mid-placeholder still
    /// delivers its text.
    fn flush(&mut self) -> String {
        self.flush_where(|_| true)
    }

    /// The held tails of content block `index`, so nothing held leaks into another block.
    fn flush_block(&mut self, index: usize) -> String {
        self.flush_where(|(_, i, _)| *i == Some(index))
    }

    fn flush_where(&mut self, keep: impl Fn(&HeldKey) -> bool) -> String {
        let mut keys: Vec<HeldKey> = self.held.keys().filter(|key| keep(key)).cloned().collect();
        keys.sort();
        let mut out = String::new();
        for key in keys {
            if let Some(data) = self.held.remove(&key) {
                out.push_str(&flush_event(&key, &data));
            }
        }
        out
    }
}

/// Rebuilds one held tail as a complete event, with the framing of the event that held it.
fn flush_event((_, _, at): &HeldKey, data: &HeldData) -> String {
    let payload = match at.as_slice() {
        [Seg::Raw] => data.text.clone(),
        _ => serde_json::from_str::<Value>(&data.frame.raw)
            .ok()
            .and_then(|mut value| set_at(&mut value, at, &data.text).map(|_| value.to_string()))
            .unwrap_or_else(|| field_event(at, &data.text).to_string()),
    };
    format!(
        "{}data: {payload}{}{}",
        data.frame.pre, data.frame.eol, data.frame.post
    )
}

/// Puts `text` at `at` in `value`; nothing to do when the path does not lead there.
fn set_at(value: &mut Value, at: &[Seg], text: &str) -> Option<()> {
    let mut cur = value;
    for seg in at {
        cur = match seg {
            Seg::Key(k) => cur.get_mut(k.as_str())?,
            Seg::Index(i) => cur.get_mut(*i)?,
            Seg::Raw => return None,
        };
    }
    *cur = Value::String(text.to_owned());
    Some(())
}

/// Byte spans (quotes included) of every string value in valid JSON, with their field path.
pub(crate) fn collect_strings(
    text: &str,
    at: &mut usize,
    path: &str,
    spans: &mut Vec<(usize, usize, String)>,
    depth: usize,
) -> Option<()> {
    let bytes = text.as_bytes();
    if depth > 128 {
        return None;
    }
    skip_space(bytes, at);
    match *bytes.get(*at)? {
        b'"' => {
            let end = string_end(bytes, *at)?;
            spans.push((*at, end, path.to_owned()));
            *at = end;
        }
        open @ (b'{' | b'[') => {
            let close = if open == b'{' { b'}' } else { b']' };
            *at += 1;
            let mut index = 0;
            loop {
                skip_space(bytes, at);
                if *bytes.get(*at)? == close {
                    *at += 1;
                    break;
                }
                let child = if open == b'{' {
                    let end = string_end(bytes, *at)?;
                    let key: String = serde_json::from_str(&text[*at..end]).ok()?;
                    *at = end;
                    skip_space(bytes, at);
                    if *bytes.get(*at)? != b':' {
                        return None;
                    }
                    *at += 1;
                    format!("{path}.{key}")
                } else {
                    format!("{path}[{index}]")
                };
                index += 1;
                collect_strings(text, at, &child, spans, depth + 1)?;
                skip_space(bytes, at);
                if *bytes.get(*at)? == b',' {
                    *at += 1;
                }
            }
        }
        _ => {
            while bytes
                .get(*at)
                .is_some_and(|b| !matches!(b, b',' | b']' | b'}') && !b.is_ascii_whitespace())
            {
                *at += 1;
            }
        }
    }
    Some(())
}

fn skip_space(bytes: &[u8], at: &mut usize) {
    while bytes.get(*at).is_some_and(u8::is_ascii_whitespace) {
        *at += 1;
    }
}

/// The offset just past the string that opens at `start`.
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    if *bytes.get(start)? != b'"' {
        return None;
    }
    let mut i = start + 1;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
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

    const FALCON: &str = "[[REIKI_TERM_aaaaaaaaaaaaaaaa]]";

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

    /// A complete Anthropic `content_block_delta` event.
    fn cb_delta(index: usize, delta_type: &str, field: &str, text: &str) -> String {
        let mut delta = Map::new();
        delta.insert("type".to_owned(), Value::String(delta_type.to_owned()));
        delta.insert(field.to_owned(), Value::String(text.to_owned()));
        let payload = serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": Value::Object(delta),
        });
        format!("event: content_block_delta\ndata: {payload}\n\n")
    }

    fn cb_stop(index: usize) -> String {
        format!(
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n"
        )
    }

    /// The `(index, delta type, text)` of every `content_block_delta` in `stream`, in order.
    fn anthropic_deltas(stream: &[u8]) -> Vec<(usize, String, String)> {
        String::from_utf8_lossy(stream)
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .filter(|value| value["type"] == "content_block_delta")
            .map(|value| {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                let kind = value["delta"]["type"].as_str().unwrap_or("").to_owned();
                let text = value["delta"]["text"]
                    .as_str()
                    .or_else(|| value["delta"]["partial_json"].as_str())
                    .unwrap_or("")
                    .to_owned();
                (index, kind, text)
            })
            .collect()
    }

    fn run_stream(chunks: &[&[u8]]) -> Vec<u8> {
        let mut restorer = Restorer::new(1024, true);
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
        let text = format!("a {FALCON} b [[REIKI_TERM_bbbbbbbbbbbbbbbb]] c [[REIKI_ d");
        assert_eq!(
            restore_text(&text, &reverse()),
            "a Falcon b [[REIKI_TERM_bbbbbbbbbbbbbbbb]] c [[REIKI_ d"
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
        assert_eq!(incomplete_start("see [[REIKI_TERM_aaaaaaaaaaaaaaaa]"), 4);
        // Finished, unrelated or too long: nothing to hold.
        assert_eq!(incomplete_start("see [[REIKI_TERM_aaaaaaaaaaaaaaaa]]"), 35);
        // A lone bracket may start a placeholder, so it is held; a closed one is not.
        assert_eq!(incomplete_start("list [1] and ["), 13);
        assert_eq!(incomplete_start("list [1] and"), 12);
        assert_eq!(incomplete_start("json [[1, 2"), 11);
        let long = format!("x [[REIKI_{}", "A".repeat(60));
        assert_eq!(incomplete_start(&long), long.len());
    }

    #[test]
    fn a_placeholder_split_across_events_is_restored_whole() {
        let e1 = event("The [[REI");
        let e2 = event("KI_TERM_aaaaaaaaaaaaaaaa]] plan");
        let done = "data: [DONE]\n\n";
        let out = run_stream(&[e1.as_bytes(), e2.as_bytes(), done.as_bytes()]);
        assert_eq!(contents(&out), "The Falcon plan");
        assert!(String::from_utf8_lossy(&out).ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn the_result_does_not_depend_on_where_the_network_splits_the_stream() {
        let whole = [
            event("go [[REIKI_TERM_"),
            event("aaaaaaaaaaaaaaaa]] now"),
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
        let mut restorer = Restorer::new(1024, true);
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
        let mut restorer = Restorer::new(8, true);
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
        let mut restorer = Restorer::new(16, true);
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
        let mut restorer = Restorer::new(1024, true);
        restorer.start(Some("application/json"), true);
        let mut chunk = Bytes::from(FALCON.to_owned());
        restorer.feed(&mut chunk, true, &reverse());
        assert_eq!(&chunk[..], FALCON.as_bytes());
    }

    const SECRET: &str = "[[REIKI_SECRET_cccccccccccccccc]]";

    fn secret_reverse() -> HashMap<String, String> {
        HashMap::from([
            (SECRET.to_owned(), "tok-123".to_owned()),
            (FALCON.to_owned(), "Falcon".to_owned()),
        ])
    }

    #[test]
    fn credentials_stay_masked_in_streamed_tool_call_arguments() {
        let call = serde_json::json!({"choices": [{"delta": {"tool_calls": [
            {"function": {"arguments": format!("curl {SECRET} {FALCON}")}}
        ]}}]});
        let text = serde_json::json!({"choices": [{"delta": {"content": SECRET}}]});
        let stream = format!("data: {call}\n\ndata: {text}\n\n");
        let mut restorer = Restorer::new(4096, false);
        restorer.start(Some("text/event-stream"), false);
        let mut chunk = Bytes::from(stream);
        restorer.feed(&mut chunk, true, &secret_reverse());
        let out = String::from_utf8(chunk.to_vec()).unwrap();
        assert!(
            out.contains(&format!("curl {SECRET} Falcon")),
            "tool-call arguments stayed masked"
        );
        assert!(
            out.contains("\"content\":\"tok-123\""),
            "the text delta was restored"
        );
    }

    #[test]
    fn a_tool_call_declared_by_type_keeps_credentials_masked() {
        let body = serde_json::json!({"content": [
            {"type": "text", "text": SECRET},
            {"input": {"command": format!("curl {SECRET}")}, "type": "tool_use"}
        ]})
        .to_string();
        let mut restorer = Restorer::new(4096, false);
        restorer.start(Some("application/json"), false);
        let mut chunk = Bytes::from(body);
        restorer.feed(&mut chunk, true, &secret_reverse());
        let out: Value = serde_json::from_slice(&chunk).unwrap();
        assert_eq!(out["content"][0]["text"], "tok-123");
        assert_eq!(
            out["content"][1]["input"]["command"],
            format!("curl {SECRET}")
        );
    }

    #[test]
    fn a_singular_tool_call_keeps_credentials_masked_in_a_whole_body() {
        let body = serde_json::json!({"items": [
            {"type": "tool_call", "arguments": {"cmd": SECRET}},
            {"type": "message", "text": SECRET}
        ]})
        .to_string();
        let mut restorer = Restorer::new(4096, false);
        restorer.start(Some("application/json"), false);
        let mut chunk = Bytes::from(body);
        restorer.feed(&mut chunk, true, &secret_reverse());
        let out: Value = serde_json::from_slice(&chunk).unwrap();
        assert_eq!(out["items"][0]["arguments"]["cmd"], SECRET);
        assert_eq!(out["items"][1]["text"], "tok-123");
    }

    #[test]
    fn camel_case_tool_calls_keep_credentials_masked() {
        let body = serde_json::json!({"candidates": [{"content": {"parts": [
            {"text": SECRET},
            {"functionCall": {"name": "run", "args": {"cmd": SECRET}}}
        ]}}]})
        .to_string();
        let mut restorer = Restorer::new(4096, false);
        restorer.start(Some("application/json"), false);
        let mut chunk = Bytes::from(body);
        restorer.feed(&mut chunk, true, &secret_reverse());
        let out: Value = serde_json::from_slice(&chunk).unwrap();
        let parts = &out["candidates"][0]["content"]["parts"];
        assert_eq!(parts[0]["text"], "tok-123");
        assert_eq!(parts[1]["functionCall"]["args"]["cmd"], SECRET);
    }

    #[test]
    fn credentials_stay_masked_in_whole_body_tool_calls_unless_allowed() {
        let body = serde_json::json!({"choices": [{"message": {
            "content": SECRET,
            "tool_calls": [{"function": {"arguments": SECRET}}]
        }}]})
        .to_string();
        let run = |tool_args| {
            let mut restorer = Restorer::new(4096, tool_args);
            restorer.start(Some("application/json"), false);
            let mut chunk = Bytes::from(body.clone());
            restorer.feed(&mut chunk, true, &secret_reverse());
            serde_json::from_slice::<Value>(&chunk).unwrap()
        };
        let masked = run(false);
        let message = &masked["choices"][0]["message"];
        assert_eq!(message["content"], "tok-123");
        assert_eq!(message["tool_calls"][0]["function"]["arguments"], SECRET);
        let open = run(true);
        assert_eq!(
            open["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "tok-123"
        );
    }

    #[test]
    fn a_rewritten_whole_body_keeps_key_order_and_layout() {
        let body = format!(
            "{{ \"zeta\" : 1.50,\n  \"alpha\": [ true, null,\t\"a\\/b \\u00e9\" ],\n  \"message\": {{\"tool_calls\": [{{\"arguments\": \"{SECRET}\"}}], \"content\": \"hi {FALCON} \\\"q\\\" {SECRET}\"}}\n}}"
        );
        let mut restorer = Restorer::new(4096, false);
        restorer.start(Some("application/json"), false);
        let mut chunk = Bytes::from(body.clone());
        restorer.feed(&mut chunk, true, &secret_reverse());
        let want = body
            .replace(&format!("hi {FALCON}"), "hi Falcon")
            .replace(&format!("\\\" {SECRET}\"}}"), "\\\" tok-123\"}");
        assert_eq!(String::from_utf8(chunk.to_vec()).unwrap(), want);
    }

    #[test]
    fn a_lone_open_bracket_at_the_end_of_an_event_is_held_and_rejoined() {
        let e1 = event("Array [");
        let e2 = event("[REIKI_TERM_aaaaaaaaaaaaaaaa]] end");
        let out = run_stream(&[e1.as_bytes(), e2.as_bytes(), b"data: [DONE]\n\n"]);
        assert_eq!(contents(&out), "Array Falcon end");
        assert!(!String::from_utf8_lossy(&out).contains("REIKI"));
    }

    #[test]
    fn a_held_tail_at_the_end_of_the_stream_is_flushed_on_done() {
        for tail in ["Array [", "Array [[", "Array [[REIKI_TERM_aa"] {
            let e = event(tail);
            let out = run_stream(&[e.as_bytes(), b"data: [DONE]\n\n"]);
            assert_eq!(contents(&out), tail, "held tail lost");
            assert!(String::from_utf8_lossy(&out).ends_with("data: [DONE]\n\n"));
        }
    }

    #[test]
    fn a_held_tail_is_flushed_when_the_last_chunk_has_no_done_event() {
        let e1 = event("see [[");
        let e2 = event("done [");
        let out = run_stream(&[e1.as_bytes(), e2.as_bytes()]);
        assert_eq!(contents(&out), "see [[done [");
    }

    #[test]
    fn a_held_tail_flushed_at_the_end_is_not_restored_into_a_partial_placeholder() {
        let e = event("left [[REIKI_TERM_aaaa");
        let out = run_stream(&[e.as_bytes(), b"data: [DONE]\n\n"]);
        assert_eq!(contents(&out), "left [[REIKI_TERM_aaaa");
    }

    #[test]
    fn a_split_secret_placeholder_after_its_first_bracket_is_restored() {
        let e1 = event("token [");
        let e2 = event("[REIKI_SECRET_cccccccccccccccc]] ok");
        let mut restorer = Restorer::new(1024, false);
        restorer.start(Some("text/event-stream"), false);
        let mut out = Vec::new();
        for part in [e1.as_bytes(), e2.as_bytes(), b"data: [DONE]\n\n"] {
            let mut chunk = Bytes::copy_from_slice(part);
            restorer.feed(&mut chunk, false, &secret_reverse());
            out.extend_from_slice(&chunk);
        }
        assert_eq!(contents(&out), "token tok-123 ok");
    }

    #[test]
    fn a_held_tail_is_flushed_before_the_done_event_and_only_once() {
        let e = event("x [");
        let done = "data: [DONE]\n\n";
        let out = String::from_utf8(run_stream(&[e.as_bytes(), done.as_bytes()])).unwrap();
        assert_eq!(out.matches("[DONE]").count(), 1);
        assert_eq!(contents(out.as_bytes()), "x [");
        let mut restorer = Restorer::new(1024, true);
        restorer.start(Some("text/event-stream"), false);
        let mut first = Bytes::from(event("x ["));
        restorer.feed(&mut first, false, &reverse());
        let mut second = Bytes::from(done);
        restorer.feed(&mut second, false, &reverse());
        let mut third = Bytes::new();
        restorer.feed(&mut third, true, &reverse());
        assert!(third.is_empty(), "nothing left to flush twice");
    }

    #[test]
    fn a_whole_body_with_nothing_to_restore_comes_back_byte_exact() {
        let body = "{ \"b\" : 2,  \"a\":\"x\", \"tool_calls\" : [ ] }";
        let mut restorer = Restorer::new(4096, false);
        restorer.start(Some("application/json"), false);
        let mut chunk = Bytes::from(body);
        restorer.feed(&mut chunk, true, &secret_reverse());
        assert_eq!(&chunk[..], body.as_bytes());
    }

    #[test]
    fn a_held_tail_does_not_leak_into_the_next_block() {
        // Block 0 ends mid-placeholder; block 1 carries the tail and more text.
        let stream = [
            cb_delta(0, "text_delta", "text", "hello [[REI"),
            cb_stop(0),
            cb_delta(1, "text_delta", "text", "KI_TERM_aaaaaaaaaaaaaaaa]] world"),
            cb_stop(1),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        ]
        .concat();
        let out = run_stream(&[stream.as_bytes()]);
        assert_eq!(
            anthropic_deltas(&out),
            vec![
                (0, "text_delta".to_owned(), "hello ".to_owned()),
                (0, "text_delta".to_owned(), "[[REI".to_owned()),
                (
                    1,
                    "text_delta".to_owned(),
                    "KI_TERM_aaaaaaaaaaaaaaaa]] world".to_owned()
                ),
            ]
        );
        assert!(
            !String::from_utf8_lossy(&out).contains("Falcon"),
            "the block-0 tail leaked into block 1"
        );
    }

    #[test]
    fn a_held_tail_flushes_before_the_stop_as_a_content_block_delta() {
        let stream = [
            cb_delta(0, "text_delta", "text", "tail [["),
            cb_stop(0),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        ]
        .concat();
        let out = String::from_utf8(run_stream(&[stream.as_bytes()])).unwrap();
        let flush = out
            .split("\n\n")
            .find(|event| event.contains("\"text\":\"[[\""))
            .expect("flush event");
        assert!(flush.starts_with("event: content_block_delta\n"), "{flush}");
        assert!(flush.contains("\"index\":0"), "{flush}");
        assert!(flush.contains("\"type\":\"text_delta\""), "{flush}");
        assert!(
            out.find("\"text\":\"[[\"").unwrap() < out.find("event: content_block_stop").unwrap(),
            "the flush must precede the stop event:\n{out}"
        );
        assert_eq!(
            anthropic_deltas(out.as_bytes()),
            vec![
                (0, "text_delta".to_owned(), "tail ".to_owned()),
                (0, "text_delta".to_owned(), "[[".to_owned()),
            ]
        );
    }

    #[test]
    fn an_input_json_delta_tail_flushes_as_input_json_delta() {
        let stream = [
            cb_delta(0, "input_json_delta", "partial_json", "{\"cmd\": \"[[REI"),
            cb_stop(0),
        ]
        .concat();
        let out = run_stream(&[stream.as_bytes()]);
        assert_eq!(
            anthropic_deltas(&out),
            vec![
                (0, "input_json_delta".to_owned(), "{\"cmd\": \"".to_owned()),
                (0, "input_json_delta".to_owned(), "[[REI".to_owned()),
            ]
        );
        let text = String::from_utf8_lossy(&out);
        let flush = text
            .split("\n\n")
            .find(|event| event.contains("\"partial_json\":\"[[REI\""))
            .expect("flush event");
        assert!(flush.starts_with("event: content_block_delta\n"), "{flush}");
        assert!(flush.contains("\"type\":\"input_json_delta\""), "{flush}");
        assert!(flush.contains("\"index\":0"), "{flush}");
    }

    #[test]
    fn a_placeholder_completed_in_the_same_block_is_joined_as_before() {
        let stream = [
            cb_delta(0, "text_delta", "text", "hi [[REI"),
            cb_delta(0, "text_delta", "text", "KI_TERM_aaaaaaaaaaaaaaaa]] there"),
            cb_stop(0),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        ]
        .concat();
        let out = run_stream(&[stream.as_bytes()]);
        assert_eq!(
            anthropic_deltas(&out),
            vec![
                (0, "text_delta".to_owned(), "hi ".to_owned()),
                (0, "text_delta".to_owned(), "Falcon there".to_owned()),
            ]
        );
    }

    #[test]
    fn anthropic_events_with_nothing_held_are_byte_exact() {
        let stream = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n"
                .to_owned(),
            cb_delta(0, "text_delta", "text", "hello world"),
            cb_stop(0),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        ]
        .concat();
        let out = run_stream(&[stream.as_bytes()]);
        assert_eq!(out, stream.as_bytes());
    }

    #[test]
    fn an_openai_held_tail_flushes_as_a_chunk_before_done() {
        let e = event("x [");
        let out = String::from_utf8(run_stream(&[e.as_bytes(), b"data: [DONE]\n\n"])).unwrap();
        let flush = out
            .split("\n\n")
            .find(|event| event.contains("\"content\":\"[\""))
            .expect("flush event");
        assert!(flush.starts_with("data: {"), "{flush}");
        assert!(flush.contains("\"choices\":[{\"delta\":{"), "{flush}");
        assert!(
            out.find("\"content\":\"[\"").unwrap() < out.find("data: [DONE]").unwrap(),
            "the flush must precede [DONE]:\n{out}"
        );
        assert_eq!(contents(out.as_bytes()), "x [");
    }
}
