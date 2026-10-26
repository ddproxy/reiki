//! Trims a parsed JSON body before it is logged: drops or summarizes chosen paths and cuts long strings.

use serde_json::{json, Map, Value};

/// A path is a top-level key (`tools`) or a JSON pointer (`/messages/0`).
fn pointer(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{}", path.replace('~', "~0").replace('/', "~1"))
    }
}

fn unescape(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// Removes the value at `path`; true when something was there.
fn remove(value: &mut Value, path: &str) -> bool {
    let pointer = pointer(path);
    let Some((parent, last)) = pointer.rsplit_once('/') else {
        return false;
    };
    let last = unescape(last);
    match value.pointer_mut(parent) {
        Some(Value::Object(map)) => map.remove(&last).is_some(),
        Some(Value::Array(items)) => match last.parse::<usize>() {
            Ok(index) if index < items.len() => {
                items.remove(index);
                true
            }
            _ => false,
        },
        _ => false,
    }
}

/// Replaces the value at `path` with a short description of it.
fn summarize(value: &mut Value, path: &str) {
    let Some(target) = value.pointer_mut(&pointer(path)) else {
        return;
    };
    let replacement = match target {
        Value::Array(items) => {
            let names: Vec<Value> = items
                .iter()
                .filter_map(|item| {
                    item.get("name")
                        .or_else(|| item.pointer("/function/name"))
                        .and_then(Value::as_str)
                        .map(|n| json!(n))
                })
                .collect();
            if !items.is_empty() && names.len() == items.len() {
                Value::Array(names)
            } else {
                json!(format!("[{} items]", items.len()))
            }
        }
        Value::Object(map) => Value::Array(map.keys().map(|k| json!(k)).collect()),
        _ => return,
    };
    *target = replacement;
}

/// Cuts every string longer than `max` bytes, noting how much was left out.
pub fn truncate_strings(value: &mut Value, max: usize) {
    match value {
        Value::String(text) if text.len() > max => {
            let mut end = max;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let left_out = text.len() - end;
            text.truncate(end);
            text.push_str(&format!("…[+{left_out} bytes]"));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| truncate_strings(v, max)),
        Value::Object(map) => map.values_mut().for_each(|v| truncate_strings(v, max)),
        _ => {}
    }
}

/// Applies `exclude` then `summarize`; returns the paths that were actually dropped.
pub fn apply(value: &mut Value, exclude: &[String], summarized: &[String]) -> Vec<String> {
    let dropped = exclude
        .iter()
        .filter(|path| remove(value, path))
        .cloned()
        .collect();
    for path in summarized {
        summarize(value, path);
    }
    dropped
}

/// The parsed `data:` payloads of an event stream, or `None` when the text has none.
pub fn sse_events(text: &str) -> Option<Vec<Value>> {
    let events: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|payload| !payload.is_empty())
        .map(|payload| {
            serde_json::from_str(payload).unwrap_or_else(|_| Value::String(payload.to_owned()))
        })
        .collect();
    (!events.is_empty()).then_some(events)
}

/// The text, finish reason and usage of a streamed completion.
pub fn assemble(events: &[Value]) -> Value {
    let mut text = String::new();
    let mut summary = Map::new();
    for event in events {
        let pieces = [
            event.pointer("/choices/0/delta/content"),
            event.pointer("/delta/text"),
        ];
        for piece in pieces.into_iter().flatten() {
            if let Some(piece) = piece.as_str() {
                text.push_str(piece);
            }
        }
        for pointer in ["/choices/0/finish_reason", "/delta/stop_reason"] {
            if let Some(reason) = event.pointer(pointer).filter(|v| !v.is_null()) {
                summary.insert("finish_reason".into(), reason.clone());
            }
        }
        if let Some(usage) = event
            .get("usage")
            .or_else(|| event.pointer("/message/usage"))
            .filter(|v| !v.is_null())
        {
            summary.insert("usage".into(), usage.clone());
        }
    }
    summary.insert("text".into(), Value::String(text));
    summary.insert("events".into(), json!(events.len()));
    Value::Object(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        json!({
            "model": "m",
            "tools": [{"function": {"name": "bash"}}, {"function": {"name": "read"}}],
            "messages": [{"role": "system", "content": "x"}, {"role": "user", "content": "hi"}]
        })
    }

    #[test]
    fn top_level_keys_and_pointers_are_dropped() {
        let mut value = body();
        let dropped = apply(
            &mut value,
            &["tools".into(), "/messages/0".into(), "missing".into()],
            &[],
        );
        assert_eq!(dropped, vec!["tools", "/messages/0"]);
        assert!(value.get("tools").is_none());
        assert_eq!(value["messages"].as_array().unwrap().len(), 1);
        assert_eq!(value["messages"][0]["role"], "user");
    }

    #[test]
    fn tools_summarize_to_their_names() {
        let mut value = body();
        apply(&mut value, &[], &["tools".into()]);
        assert_eq!(value["tools"], json!(["bash", "read"]));
    }

    #[test]
    fn arrays_without_names_summarize_to_a_count() {
        let mut value = json!({"items": [1, 2, 3]});
        apply(&mut value, &[], &["items".into()]);
        assert_eq!(value["items"], json!("[3 items]"));
    }

    #[test]
    fn long_strings_are_cut_with_a_marker() {
        let mut value = json!({"a": "é".repeat(10), "b": "short"});
        truncate_strings(&mut value, 5);
        assert_eq!(value["a"], json!("éé…[+16 bytes]"));
        assert_eq!(value["b"], json!("short"));
    }

    #[test]
    fn event_streams_parse_and_assemble() {
        let text = "data: {\"choices\":[{\"delta\":{\"content\":\"po\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ng\"},\"finish_reason\":\"stop\"}],\"usage\":{\"total_tokens\":3}}\n\ndata: [DONE]\n\n";
        let events = sse_events(text).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2], json!("[DONE]"));
        let assembled = assemble(&events);
        assert_eq!(assembled["text"], "pong");
        assert_eq!(assembled["finish_reason"], "stop");
        assert_eq!(assembled["usage"]["total_tokens"], 3);
        assert!(sse_events("{\"a\":1}").is_none());
    }
}
