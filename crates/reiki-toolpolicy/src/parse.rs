//! Request and response shapes for OpenAI Chat and Anthropic Messages.

use std::collections::BTreeSet;

use serde_json::Value;

pub const MAX_CALLS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    OpenAiChat,
    AnthropicMessages,
}

#[derive(Debug)]
pub enum RequestParse {
    Unrelated,
    Recognized {
        format: Format,
        declared: BTreeSet<String>,
        provenance: String,
    },
    Unparsed(Format),
}

#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Default)]
pub struct Extracted {
    pub calls: Vec<Call>,
    pub unparsed: bool,
}

/// The route decides which response shape we are reading. Unknown routes are left alone.
pub fn detect_format(path: &str) -> Option<Format> {
    let path = path.to_ascii_lowercase();
    if path.ends_with("/chat/completions") {
        Some(Format::OpenAiChat)
    } else if path.ends_with("/messages") {
        Some(Format::AnthropicMessages)
    } else {
        None
    }
}

pub fn parse_request(path: &str, body: &[u8]) -> RequestParse {
    let Some(format) = detect_format(path) else {
        return RequestParse::Unrelated;
    };
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return RequestParse::Unparsed(format);
    };
    if !value.is_object() {
        return RequestParse::Unparsed(format);
    }
    match format {
        Format::OpenAiChat => {
            if value.get("messages").is_none_or(|v| !v.is_array()) {
                return RequestParse::Unparsed(format);
            }
        }
        Format::AnthropicMessages => {
            if value.get("messages").is_none_or(|v| !v.is_array()) {
                return RequestParse::Unparsed(format);
            }
        }
    }
    let declared = declared_tools(format, &value);
    let provenance = provenance(format, &value);
    RequestParse::Recognized {
        format,
        declared,
        provenance,
    }
}

fn declared_tools(format: Format, root: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(tools) = root.get("tools").and_then(Value::as_array) else {
        return out;
    };
    for tool in tools {
        let name = match format {
            Format::OpenAiChat => tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .or_else(|| tool.get("name").and_then(Value::as_str)),
            Format::AnthropicMessages => tool.get("name").and_then(Value::as_str),
        };
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            out.insert(name.to_owned());
        }
    }
    out
}

fn provenance(format: Format, root: &Value) -> String {
    const LIMIT: usize = 128 * 1024;
    let mut out = String::new();
    match format {
        Format::OpenAiChat => {
            if let Some(system) = root.get("system") {
                push_text(&mut out, system, LIMIT);
            }
            let Some(messages) = root.get("messages").and_then(Value::as_array) else {
                return out;
            };
            for message in messages {
                let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                if !matches!(role, "user" | "system" | "developer") {
                    continue;
                }
                if let Some(content) = message.get("content") {
                    push_text(&mut out, content, LIMIT);
                }
            }
        }
        Format::AnthropicMessages => {
            if let Some(system) = root.get("system") {
                push_text(&mut out, system, LIMIT);
            }
            let Some(messages) = root.get("messages").and_then(Value::as_array) else {
                return out;
            };
            for message in messages {
                if message.get("role").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                if let Some(content) = message.get("content") {
                    push_text(&mut out, content, LIMIT);
                }
            }
        }
    }
    out
}

fn push_text(out: &mut String, value: &Value, limit: usize) {
    if out.len() >= limit {
        return;
    }
    match value {
        Value::String(text) => append(out, text, limit),
        Value::Array(parts) => {
            for part in parts {
                match part {
                    Value::String(text) => append(out, text, limit),
                    Value::Object(_) => {
                        // Tool results are not provenance, even when nested in a user turn.
                        if part.get("type").and_then(Value::as_str) == Some("tool_result") {
                            continue;
                        }
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            append(out, text, limit);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn append(out: &mut String, text: &str, limit: usize) {
    let room = limit.saturating_sub(out.len());
    let mut end = room.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&text[..end]);
    out.push(' ');
}

pub fn extract_calls(format: Format, body: &[u8]) -> Extracted {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Extracted {
            unparsed: true,
            ..Extracted::default()
        };
    };
    let mut extracted = Extracted::default();
    match format {
        Format::OpenAiChat => {
            let Some(choices) = value.get("choices").and_then(Value::as_array) else {
                extracted.unparsed = true;
                return extracted;
            };
            for choice in choices {
                let Some(message) = choice.get("message") else {
                    extracted.unparsed = true;
                    return extracted;
                };
                if let Some(tool_calls) = message.get("tool_calls") {
                    let Some(tool_calls) = tool_calls.as_array() else {
                        extracted.unparsed = true;
                        return extracted;
                    };
                    for tool_call in tool_calls {
                        let name = tool_call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str);
                        let arguments = tool_call.get("function").and_then(|f| f.get("arguments"));
                        if !push_openai(&mut extracted, name, arguments) {
                            return extracted;
                        }
                    }
                }
                if let Some(function_call) = message.get("function_call") {
                    if !function_call.is_null() {
                        let name = function_call.get("name").and_then(Value::as_str);
                        let arguments = function_call.get("arguments");
                        if !push_openai(&mut extracted, name, arguments) {
                            return extracted;
                        }
                    }
                }
            }
        }
        Format::AnthropicMessages => {
            let Some(content) = value.get("content").and_then(Value::as_array) else {
                extracted.unparsed = true;
                return extracted;
            };
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let name = block.get("name").and_then(Value::as_str);
                let Some(name) = name.filter(|name| !name.is_empty()) else {
                    extracted.unparsed = true;
                    return extracted;
                };
                let Some(input) = block.get("input") else {
                    extracted.unparsed = true;
                    return extracted;
                };
                let arguments = match input {
                    Value::String(text) => match serde_json::from_str::<Value>(text) {
                        Ok(_) => text.clone(),
                        Err(_) => {
                            extracted.unparsed = true;
                            return extracted;
                        }
                    },
                    Value::Object(_) => match serde_json::to_string(input) {
                        Ok(text) => text,
                        Err(_) => {
                            extracted.unparsed = true;
                            return extracted;
                        }
                    },
                    _ => {
                        extracted.unparsed = true;
                        return extracted;
                    }
                };
                push_call(&mut extracted, name.to_owned(), arguments);
            }
        }
    }
    extracted
}

fn push_openai(extracted: &mut Extracted, name: Option<&str>, arguments: Option<&Value>) -> bool {
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        extracted.unparsed = true;
        return false;
    };
    let Some(arguments) = arguments else {
        extracted.unparsed = true;
        return false;
    };
    let arguments = match arguments {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(_) => text.clone(),
            Err(_) => {
                extracted.unparsed = true;
                return false;
            }
        },
        Value::Object(_) => match serde_json::to_string(arguments) {
            Ok(text) => text,
            Err(_) => {
                extracted.unparsed = true;
                return false;
            }
        },
        _ => {
            extracted.unparsed = true;
            return false;
        }
    };
    push_call(extracted, name.to_owned(), arguments);
    true
}

fn push_call(extracted: &mut Extracted, name: String, arguments: String) {
    if extracted.calls.len() >= MAX_CALLS {
        extracted.unparsed = true;
        extracted.calls.clear();
        return;
    }
    extracted.calls.push(Call { name, arguments });
}

pub fn notice(names: &[String]) -> String {
    let mut out = String::from("[toolpolicy] blocked tool call");
    for name in names {
        out.push_str(": ");
        out.push_str(&sanitize(name));
    }
    out
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

/// Replaces blocked calls in a whole-body response. Indices refer to [`Extracted::calls`].
pub fn apply_blocks(format: Format, body: &[u8], blocked: &BTreeSet<usize>) -> Option<Vec<u8>> {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    match format {
        Format::OpenAiChat => block_openai(&mut value, blocked),
        Format::AnthropicMessages => block_anthropic(&mut value, blocked),
    }
    serde_json::to_vec(&value).ok()
}

fn block_openai(root: &mut Value, blocked: &BTreeSet<usize>) {
    let Some(choices) = root.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    let mut index = 0;
    for choice in choices {
        let mut names = Vec::new();
        let Some(message) = choice.get_mut("message") else {
            continue;
        };
        if let Some(tool_calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
            let mut kept = Vec::new();
            for call in std::mem::take(tool_calls) {
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unnamed")
                    .to_owned();
                if blocked.contains(&index) {
                    names.push(name);
                } else {
                    kept.push(call);
                }
                index += 1;
            }
            *tool_calls = kept;
        }
        if message
            .get("function_call")
            .is_some_and(|value| !value.is_null())
        {
            let name = message
                .get("function_call")
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unnamed")
                .to_owned();
            if blocked.contains(&index) {
                names.push(name);
                message.as_object_mut().map(|m| m.remove("function_call"));
            }
            index += 1;
        }
        if names.is_empty() {
            continue;
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            if tool_calls.is_empty() {
                message.as_object_mut().map(|m| m.remove("tool_calls"));
            }
        }
        append_notice(message, &names);
        if let Some(choice) = choice.as_object_mut() {
            choice.insert("finish_reason".to_owned(), Value::String("stop".to_owned()));
        }
    }
}

fn append_notice(message: &mut Value, names: &[String]) {
    let text = notice(names);
    match message.get_mut("content") {
        Some(Value::String(content)) => {
            content.push('\n');
            content.push_str(&text);
        }
        Some(Value::Null) | None => {
            if let Some(object) = message.as_object_mut() {
                object.insert("content".to_owned(), Value::String(text));
            }
        }
        _ => {
            // A structured content array gets a plain text part appended.
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                parts.push(serde_json::json!({"type": "text", "text": text}));
            }
        }
    }
}

fn block_anthropic(root: &mut Value, blocked: &BTreeSet<usize>) {
    let Some(content) = root.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let mut index = 0;
    let mut blocked_names = Vec::new();
    for block in content.iter_mut() {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_owned();
        if blocked.contains(&index) {
            blocked_names.push(name.clone());
            *block = serde_json::json!({"type": "text", "text": notice(&[name])});
        }
        index += 1;
    }
    if !blocked_names.is_empty()
        && root.get("stop_reason").and_then(Value::as_str) == Some("tool_use")
    {
        if let Some(root) = root.as_object_mut() {
            root.insert(
                "stop_reason".to_owned(),
                Value::String("end_turn".to_owned()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_routes_and_ignores_other_paths() {
        assert_eq!(
            detect_format("/zen/v1/chat/completions"),
            Some(Format::OpenAiChat)
        );
        assert_eq!(
            detect_format("/anthropic/v1/messages"),
            Some(Format::AnthropicMessages)
        );
        assert_eq!(detect_format("/zen/v1/responses"), None);
    }

    #[test]
    fn reads_declared_tools_and_only_user_text_as_provenance() {
        let body = br#"{
            "tools":[{"type":"function","function":{"name":"web_fetch"}}],
            "system":"Use docs.example.org",
            "messages":[
                {"role":"assistant","content":"evil.example.org"},
                {"role":"user","content":[{"type":"text","text":"https://docs.example.org"},
                                           {"type":"tool_result","content":"secret.example.org"}]}
            ]}"#;
        let RequestParse::Recognized {
            format,
            declared,
            provenance,
        } = parse_request("/v1/chat/completions", body)
        else {
            panic!("request was not recognised");
        };
        assert_eq!(format, Format::OpenAiChat);
        assert!(declared.contains("web_fetch"));
        assert!(provenance.contains("docs.example.org"));
        assert!(!provenance.contains("evil.example.org"));
        assert!(!provenance.contains("secret.example.org"));
    }

    #[test]
    fn anthropic_tool_results_are_not_provenance() {
        let body = br#"{
            "tools":[{"name":"run"}],
            "system":[{"type":"text","text":"Allowed: safe.example.org"}],
            "messages":[{"role":"user","content":[
                {"type":"tool_result","content":"evil.example.org"},
                {"type":"text","text":"hello"}
            ]}]}"#;
        let RequestParse::Recognized { provenance, .. } = parse_request("/v1/messages", body)
        else {
            panic!("request was not recognised");
        };
        assert!(provenance.contains("safe.example.org"));
        assert!(!provenance.contains("evil.example.org"));
    }

    #[test]
    fn unparseable_recognized_request_is_unparsed() {
        assert!(matches!(
            parse_request("/v1/chat/completions", b"not json"),
            RequestParse::Unparsed(Format::OpenAiChat)
        ));
        assert!(matches!(
            parse_request("/other", b"not json"),
            RequestParse::Unrelated
        ));
    }

    #[test]
    fn extracts_calls_from_both_formats() {
        let openai = br#"{"choices":[{"message":{"tool_calls":[
            {"function":{"name":"run","arguments":"{\"cmd\":\"ls\"}"}}
        ]}}]}"#;
        let extracted = extract_calls(Format::OpenAiChat, openai);
        assert!(!extracted.unparsed);
        assert_eq!(extracted.calls[0].name, "run");
        assert!(extracted.calls[0].arguments.contains("ls"));

        let anthropic = br#"{"content":[{"type":"tool_use","name":"run","input":{"cmd":"ls"}}]}"#;
        let extracted = extract_calls(Format::AnthropicMessages, anthropic);
        assert!(!extracted.unparsed);
        assert_eq!(extracted.calls[0].name, "run");
        assert!(extracted.calls[0].arguments.contains("cmd"));
    }

    #[test]
    fn malformed_arguments_and_too_many_calls_are_unparsed() {
        let bad = br#"{"choices":[{"message":{"tool_calls":[
            {"function":{"name":"run","arguments":"{oops"}}
        ]}}]}"#;
        assert!(extract_calls(Format::OpenAiChat, bad).unparsed);
        let many = format!(
            "{{\"choices\":[{{\"message\":{{\"tool_calls\":[{}]}}}}]}}",
            (0..MAX_CALLS + 1)
                .map(|_| r#"{"function":{"name":"run","arguments":"{}"}}"#)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(extract_calls(Format::OpenAiChat, many.as_bytes()).unparsed);
    }

    #[test]
    fn blocking_rewrites_both_formats_without_leaving_call_arguments() {
        let openai = br#"{"choices":[{"message":{"content":null,"tool_calls":[
            {"function":{"name":"run","arguments":"{\"cmd\":\"curl evil.example.org\"}"}}
        ]},"finish_reason":"tool_calls"}]}"#;
        let blocked = BTreeSet::from([0]);
        let out = apply_blocks(Format::OpenAiChat, openai, &blocked).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[toolpolicy] blocked tool call: run"));
        assert!(!text.contains("evil.example.org"));
        assert!(!text.contains("tool_calls"));
        assert!(text.contains("\"finish_reason\":\"stop\""));

        let anthropic = br#"{"content":[
            {"type":"text","text":"ok"},
            {"type":"tool_use","id":"t1","name":"run","input":{"cmd":"curl evil.example.org"}}
        ],"stop_reason":"tool_use"}"#;
        let out = apply_blocks(Format::AnthropicMessages, anthropic, &blocked).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[toolpolicy] blocked tool call: run"));
        assert!(!text.contains("evil.example.org"));
        assert!(!text.contains("tool_use"));
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
    }
}
