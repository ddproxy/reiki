use std::collections::BTreeMap;

use axum::body::Bytes;

/// Rewrite a JSON object body once, before any key is tried, so every replay
/// sends the same bytes. The body is scanned at the byte level and untouched
/// members are copied verbatim: re-serialising through a JSON parser would
/// change numbers and unicode escapes that must round-trip exactly.
pub fn rewrite_body(body: Bytes, aliases: &BTreeMap<String, String>, drop: &[String]) -> Bytes {
    if aliases.is_empty() && drop.is_empty() {
        return body;
    }
    match rewrite(&body, aliases, drop) {
        Some(rewritten) => Bytes::from(rewritten),
        None => body,
    }
}

struct Member {
    start: usize,
    value_start: usize,
    value_end: usize,
    key: Vec<u8>,
    is_string: bool,
}

enum Action {
    Keep,
    Drop,
    Replace(Vec<u8>),
}

fn rewrite(body: &[u8], aliases: &BTreeMap<String, String>, drop: &[String]) -> Option<Vec<u8>> {
    let members = top_level_members(body)?;
    let mut changed = false;
    let actions: Vec<Action> = members
        .iter()
        .map(|member| {
            if drop.iter().any(|name| name.as_bytes() == member.key) {
                changed = true;
                return Action::Drop;
            }
            if member.key == b"model" && member.is_string {
                let raw = &body[member.value_start + 1..member.value_end - 1];
                if let Some(mapped) = std::str::from_utf8(raw)
                    .ok()
                    .and_then(|m| aliases.get(m).or_else(|| aliases.get("*")))
                {
                    changed = true;
                    return Action::Replace(json_string(mapped));
                }
            }
            Action::Keep
        })
        .collect();
    if !changed {
        return None;
    }

    let mut out = Vec::with_capacity(body.len());
    out.push(b'{');
    let mut first = true;
    for (member, action) in members.iter().zip(actions) {
        if matches!(action, Action::Drop) {
            continue;
        }
        if !first {
            out.push(b',');
        }
        first = false;
        out.extend_from_slice(&body[member.start..member.value_start]);
        match action {
            Action::Replace(value) => out.extend_from_slice(&value),
            _ => out.extend_from_slice(&body[member.value_start..member.value_end]),
        }
    }
    out.push(b'}');
    Some(out)
}

fn top_level_members(body: &[u8]) -> Option<Vec<Member>> {
    let mut i = skip_ws(body, 0);
    if body.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut members = Vec::new();
    loop {
        i = skip_ws(body, i);
        if body.get(i) == Some(&b'}') {
            return (skip_ws(body, i + 1) == body.len()).then_some(members);
        }
        if body.get(i) != Some(&b'"') {
            return None;
        }
        let start = i;
        let key_end = scan_string(body, i)?;
        let key = body[start + 1..key_end - 1].to_vec();
        i = skip_ws(body, key_end);
        if body.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(body, i + 1);
        let (value_end, is_string) = scan_value(body, i)?;
        members.push(Member {
            start,
            value_start: i,
            value_end,
            key,
            is_string,
        });
        i = skip_ws(body, value_end);
        match body.get(i) {
            Some(b',') => i += 1,
            Some(b'}') => return (skip_ws(body, i + 1) == body.len()).then_some(members),
            _ => return None,
        }
    }
}

fn scan_value(body: &[u8], i: usize) -> Option<(usize, bool)> {
    match body.get(i)? {
        b'"' => Some((scan_string(body, i)?, true)),
        b'{' | b'[' => Some((scan_container(body, i)?, false)),
        _ => {
            let mut j = i;
            while j < body.len()
                && !matches!(body[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                j += 1;
            }
            (j > i).then_some((j, false))
        }
    }
}

/// Index just past the closing quote of the string that starts at `i`.
fn scan_string(body: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < body.len() {
        match body[j] {
            b'"' => return Some(j + 1),
            b'\\' => j += 2,
            _ => j += 1,
        }
    }
    None
}

/// Index just past the object or array that starts at `i`, skipping strings so
/// brackets inside them do not count.
fn scan_container(body: &[u8], i: usize) -> Option<usize> {
    let mut stack = Vec::new();
    let mut j = i;
    while j < body.len() {
        match body[j] {
            b'"' => {
                j = scan_string(body, j)?;
                continue;
            }
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                if stack.pop()? != body[j] {
                    return None;
                }
                if stack.is_empty() {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn skip_ws(body: &[u8], mut i: usize) -> usize {
    while i < body.len() && matches!(body[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn json_string(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'"');
    for &byte in value.as_bytes() {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b if b < 0x20 => out.extend_from_slice(format!("\\u{b:04x}").as_bytes()),
            b => out.push(b),
        }
    }
    out.push(b'"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn run(body: &str, aliases: &BTreeMap<String, String>, drop: &[&str]) -> Bytes {
        let drop: Vec<String> = drop.iter().map(|d| (*d).to_owned()).collect();
        rewrite_body(Bytes::copy_from_slice(body.as_bytes()), aliases, &drop)
    }

    #[test]
    fn a_wildcard_alias_catches_models_without_their_own_entry() {
        let map = aliases(&[("a", "x"), ("*", "fallback/free")]);
        assert_eq!(
            run(r#"{"model":"a"}"#, &map, &[]),
            Bytes::from_static(br#"{"model":"x"}"#)
        );
        assert_eq!(
            run(r#"{"model":"other"}"#, &map, &[]),
            Bytes::from_static(br#"{"model":"fallback/free"}"#)
        );
        assert_eq!(
            run(r#"{"messages":[]}"#, &map, &[]),
            Bytes::from_static(br#"{"messages":[]}"#),
            "no model, nothing to map"
        );
    }

    #[test]
    fn an_alias_hit_replaces_the_model() {
        let out = run(
            r#"{"model":"gpt-4o","messages":[]}"#,
            &aliases(&[("gpt-4o", "opencode/gpt-4o")]),
            &[],
        );
        assert_eq!(
            out,
            Bytes::from_static(br#"{"model":"opencode/gpt-4o","messages":[]}"#)
        );
    }

    #[test]
    fn an_alias_miss_leaves_the_body_untouched() {
        let body = r#"{"model":"gpt-4o-mini","messages":[]}"#;
        let out = run(body, &aliases(&[("gpt-4o", "opencode/gpt-4o")]), &[]);
        assert_eq!(out, Bytes::copy_from_slice(body.as_bytes()));
    }

    #[test]
    fn a_non_string_model_is_left_alone() {
        let body = r#"{"model":123,"messages":[]}"#;
        let out = run(body, &aliases(&[("123", "x")]), &[]);
        assert_eq!(out, Bytes::copy_from_slice(body.as_bytes()));
    }

    #[test]
    fn a_non_json_body_is_left_alone() {
        let body = "not json at all";
        let out = run(body, &aliases(&[("gpt-4o", "x")]), &["stream_options"]);
        assert_eq!(out, Bytes::copy_from_slice(body.as_bytes()));
    }

    #[test]
    fn a_top_level_array_is_left_alone() {
        let body = r#"[{"model":"gpt-4o"}]"#;
        let out = run(body, &aliases(&[("gpt-4o", "x")]), &[]);
        assert_eq!(out, Bytes::copy_from_slice(body.as_bytes()));
    }

    #[test]
    fn escaped_and_unicode_surrounding_fields_round_trip() {
        let body = r#"{"note":"caf\u00e9 \"quoted\"\nline","model":"gpt-4o","big":12345678901234567890,"emoji":"😀"}"#;
        let out = run(body, &aliases(&[("gpt-4o", "zen")]), &[]);
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            r#"{"note":"caf\u00e9 \"quoted\"\nline","model":"zen","big":12345678901234567890,"emoji":"😀"}"#
        );
    }

    #[test]
    fn drop_params_removes_a_top_level_key_only() {
        let body = r#"{"options":{"stream_options":true},"stream_options":2,"model":"gpt-4o"}"#;
        let out = run(body, &BTreeMap::new(), &["stream_options"]);
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            r#"{"options":{"stream_options":true},"model":"gpt-4o"}"#
        );
    }

    #[test]
    fn an_alias_and_a_drop_apply_together() {
        let out = run(
            r#"{"model":"gpt-4o","stream_options":{"include_usage":true},"temperature":0}"#,
            &aliases(&[("gpt-4o", "zen")]),
            &["stream_options"],
        );
        assert_eq!(
            String::from_utf8(out.to_vec()).unwrap(),
            r#"{"model":"zen","temperature":0}"#
        );
    }

    #[test]
    fn a_large_body_is_rewritten() {
        let filler = "x".repeat(4 * 1024 * 1024);
        let body = format!(r#"{{"model":"gpt-4o","blob":"{filler}"}}"#);
        let out = run(&body, &aliases(&[("gpt-4o", "zen")]), &[]);
        let expected = format!(r#"{{"model":"zen","blob":"{filler}"}}"#);
        assert_eq!(out, Bytes::from(expected));
    }

    #[test]
    fn without_settings_the_same_bytes_are_returned() {
        let body = Bytes::from_static(br#"{"model":"gpt-4o","stream_options":{}}"#);
        let out = rewrite_body(body.clone(), &BTreeMap::new(), &[]);
        assert_eq!(out, body);
    }

    #[test]
    fn a_model_with_an_escape_is_not_matched() {
        let body = r#"{"model":"gpt\u002d4o"}"#;
        let out = run(body, &aliases(&[("gpt-4o", "zen")]), &[]);
        assert_eq!(out, Bytes::copy_from_slice(body.as_bytes()));
    }
}
