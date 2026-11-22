//! Bounded extraction of provider token usage from a response body.
//!
//! The proxy watches a non-compressed body as it streams by: JSON is read whole
//! (up to [`MAX_JSON_BODY`]), while an event stream is parsed one line at a time
//! (up to [`MAX_SSE_LINE`] per line). Nothing is buffered beyond those caps.

use serde_json::Value;

/// The most JSON response body kept to read `usage` from.
const MAX_JSON_BODY: usize = 1024 * 1024;
/// The longest event-stream line kept while waiting for its newline.
const MAX_SSE_LINE: usize = 64 * 1024;

/// Token counts seen in one response. Completion is idempotent; dropping the
/// owner without calling [`Usage::finish`] is safe but leaves no total.
#[derive(Debug)]
pub(crate) struct Usage {
    is_sse: bool,
    /// A JSON body, or the unfinished line of an event stream.
    buffer: Vec<u8>,
    /// Dropping the rest of an over-long event line up to its newline.
    discarding: bool,
    exceeded: bool,
    /// A line or body passed a cap, so any token count from it is incomplete.
    unaccounted: bool,
    input: Option<u64>,
    output: Option<u64>,
    total: Option<u64>,
    done: bool,
}

impl Usage {
    pub(crate) fn new(is_sse: bool) -> Self {
        Self {
            is_sse,
            buffer: Vec::new(),
            discarding: false,
            exceeded: false,
            unaccounted: false,
            input: None,
            output: None,
            total: None,
            done: false,
        }
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        if self.done {
            return;
        }
        if self.is_sse {
            self.feed_sse(chunk);
        } else if !self.exceeded {
            if self.buffer.len() + chunk.len() > MAX_JSON_BODY {
                self.exceeded = true;
                self.unaccounted = true;
                self.buffer.clear();
            } else {
                self.buffer.extend_from_slice(chunk);
            }
        }
    }

    fn feed_sse(&mut self, chunk: &[u8]) {
        for part in chunk.split_inclusive(|b| *b == b'\n') {
            let complete = part.ends_with(b"\n");
            if !self.discarding {
                if self.buffer.len() + part.len() > MAX_SSE_LINE + usize::from(complete) {
                    self.buffer.clear();
                    self.discarding = true;
                    self.unaccounted = true;
                } else {
                    self.buffer.extend_from_slice(part);
                }
            }
            if complete {
                if !self.discarding {
                    let line = std::mem::take(&mut self.buffer);
                    self.read_event_line(&line);
                }
                self.buffer.clear();
                self.discarding = false;
            }
        }
    }

    /// Usage can arrive in any event that carries it: one final chunk for OpenAI
    /// style, `message_start` and `message_delta` for Anthropic style. Later
    /// values win per field.
    fn read_event_line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let Some(data) = text.trim().strip_prefix("data:") else {
            return;
        };
        if !data.contains("\"usage\"") {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
            self.take(usage_in(&value));
        }
    }

    fn take(&mut self, (input, output, total): (Option<u64>, Option<u64>, Option<u64>)) {
        self.input = input.or(self.input);
        self.output = output.or(self.output);
        self.total = total.or(self.total);
    }

    /// Parses whatever is still buffered, once.
    pub(crate) fn finish(&mut self) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        if self.is_sse {
            let rest = std::mem::take(&mut self.buffer);
            if !self.discarding {
                self.read_event_line(&rest);
            }
        } else if !self.exceeded
            && let Ok(value) = serde_json::from_slice::<Value>(&self.buffer)
        {
            self.take(usage_in(&value));
        }
    }

    /// True when a body or event line passed a parse cap. Such a response may still yield a
    /// partial count, but it must not be mistaken for a complete token measurement.
    pub(crate) fn unaccounted(&self) -> bool {
        self.unaccounted
    }

    /// Prompt + completion when either was reported, otherwise `total_tokens`.
    pub(crate) fn total(&mut self) -> Option<u64> {
        self.finish();
        if self.input.is_some() || self.output.is_some() {
            Some(
                self.input
                    .unwrap_or(0)
                    .saturating_add(self.output.unwrap_or(0)),
            )
        } else {
            self.total
        }
    }
}

/// Input, output and total tokens from a response or an event, in OpenAI
/// (`prompt_tokens`/`completion_tokens`/`total_tokens`) or Anthropic
/// (`input_tokens`/`output_tokens`) naming. Anthropic's `message_start` nests
/// them under `message.usage`.
fn usage_in(value: &Value) -> (Option<u64>, Option<u64>, Option<u64>) {
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"));
    let Some(usage) = usage else {
        return (None, None, None);
    };
    let count = |names: &[&str]| names.iter().find_map(|n| usage.get(*n)?.as_u64());
    (
        count(&["prompt_tokens", "input_tokens"]),
        count(&["completion_tokens", "output_tokens"]),
        usage.get("total_tokens").and_then(Value::as_u64),
    )
}

#[cfg(test)]
mod tests {
    use super::Usage;

    fn json(text: &str) -> Option<u64> {
        let mut usage = Usage::new(false);
        usage.feed(text.as_bytes());
        usage.total()
    }

    fn sse(chunks: &[&str]) -> Option<u64> {
        let mut usage = Usage::new(true);
        for chunk in chunks {
            usage.feed(chunk.as_bytes());
        }
        usage.total()
    }

    #[test]
    fn reads_openai_json() {
        assert_eq!(
            json(r#"{"usage":{"prompt_tokens":6,"completion_tokens":4,"total_tokens":10}}"#),
            Some(10)
        );
    }

    #[test]
    fn reads_anthropic_json() {
        assert_eq!(
            json(r#"{"usage":{"input_tokens":3,"output_tokens":7}}"#),
            Some(10)
        );
        assert_eq!(
            json(r#"{"message":{"usage":{"input_tokens":3,"output_tokens":7}}}"#),
            Some(10)
        );
    }

    #[test]
    fn falls_back_to_total_tokens() {
        assert_eq!(json(r#"{"usage":{"total_tokens":42}}"#), Some(42));
    }

    #[test]
    fn reads_a_split_sse_line() {
        assert_eq!(
            sse(&[
                "data: {\"usage\":{\"prompt_tokens\":3,",
                "\"completion_tokens\":4}}\n\ndata: [DONE]\n\n",
            ]),
            Some(7)
        );
    }

    #[test]
    fn reads_anthropic_start_and_delta() {
        assert_eq!(
            sse(&[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":22}}\n\n",
            ]),
            Some(33)
        );
    }

    #[test]
    fn reads_a_final_line_without_a_newline() {
        assert_eq!(sse(&["data: {\"usage\":{\"total_tokens\":9}}"]), Some(9));
    }

    #[test]
    fn no_usage_is_none() {
        assert_eq!(json(r#"{"ok":true}"#), None);
        assert_eq!(sse(&["data: {\"ok\":true}\n\n"]), None);
    }

    #[test]
    fn oversized_json_is_ignored() {
        let mut text = String::from("{\"usage\":{\"total_tokens\":1},\"pad\":\"");
        text.push_str(&"x".repeat(1024 * 1024));
        text.push_str("\"}");
        let mut usage = Usage::new(false);
        usage.feed(text.as_bytes());
        assert_eq!(usage.total(), None);
        assert!(usage.unaccounted());
    }

    #[test]
    fn oversized_sse_line_is_ignored() {
        let mut line = String::from("data: {\"usage\":{\"total_tokens\":1},\"pad\":\"");
        line.push_str(&"x".repeat(64 * 1024));
        line.push_str("\"}\n\n");
        let mut usage = Usage::new(true);
        usage.feed(line.as_bytes());
        assert_eq!(usage.total(), None);
        assert!(usage.unaccounted());
    }

    #[test]
    fn ordinary_bodies_are_accounted() {
        let mut usage = Usage::new(false);
        usage.feed(br#"{"usage":{"total_tokens":1}}"#);
        assert_eq!(usage.total(), Some(1));
        assert!(!usage.unaccounted());
    }
}
