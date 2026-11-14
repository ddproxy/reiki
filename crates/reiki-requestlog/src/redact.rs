use std::borrow::Cow;

use reiki_redact::Span;
use serde_json::Value;

const MASK: &str = "[REDACTED]";
const PRIVATE_KEY_MASK: &str = "[REDACTED PRIVATE KEY]";

/// Masks credential-shaped substrings; borrows the input when nothing matched.
pub fn secrets(text: &str) -> Cow<'_, str> {
    mask_spans(text, spans_of(text))
}

/// Like [`secrets`], for text that was cut short: a credential cut off at the end is masked too.
pub fn secrets_cut(text: &str, cut: bool) -> Cow<'_, str> {
    let mut spans = spans_of(text);
    if cut {
        if let Some(start) = reiki_redact::partial_tail(text) {
            spans.push(Span {
                start,
                end: text.len(),
                private_key: false,
            });
            spans = merge(spans);
        }
    }
    mask_spans(text, spans)
}

fn mask_spans(text: &str, spans: Vec<Span>) -> Cow<'_, str> {
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for span in spans {
        out.push_str(&text[copied..span.start]);
        out.push_str(mark(&span));
        copied = span.end;
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

fn mark(span: &Span) -> &'static str {
    if span.private_key {
        PRIVATE_KEY_MASK
    } else {
        MASK
    }
}

fn spans_of(text: &str) -> Vec<Span> {
    let mut spans = reiki_redact::scan(text);
    let encoded = encoded_spans(text);
    if !encoded.is_empty() {
        spans.extend(encoded);
        spans = merge(spans);
    }
    spans
}

/// Sorted, with overlapping spans joined.
fn merge(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            Some(last) if span.start < last.end => {
                last.end = last.end.max(span.end);
                last.private_key |= span.private_key;
            }
            _ => out.push(span),
        }
    }
    out
}

/// A run must be this long before a base64 decode is tried.
const MIN_BASE64_RUN: usize = 24;
/// Characters decoded from the start of a run; a key sits at the start.
const MAX_DECODED_CHARS: usize = 256;
/// Characters of a percent-encoded run that are decoded.
const MAX_PERCENT_RUN: usize = 4096;

/// Base64 or percent-encoded values that decode to a credential, whole-value only.
fn encoded_spans(text: &str) -> Vec<Span> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let has_percent = bytes.contains(&b'%');
    let mut i = 0;
    while i < bytes.len() {
        if !is_run_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_run_byte(bytes[i]) {
            i += 1;
        }
        let run = &bytes[start..i];
        if has_percent && run.contains(&b'%') {
            let head = &run[..run.len().min(MAX_PERCENT_RUN)];
            let decoded = percent_decode(head);
            if !reiki_redact::scan(&String::from_utf8_lossy(&decoded)).is_empty() {
                spans.push(Span {
                    start,
                    end: i,
                    private_key: false,
                });
                continue;
            }
        }
        let core = run
            .iter()
            .position(|b| !is_base64_byte(*b))
            .unwrap_or(run.len());
        if core >= MIN_BASE64_RUN {
            let head = &run[..core.min(MAX_DECODED_CHARS)];
            if let Some(private_key) = decoded_credential(head) {
                let padding = bytes[i..]
                    .iter()
                    .take(2)
                    .take_while(|b| **b == b'=')
                    .count();
                spans.push(Span {
                    start,
                    end: start + core + if core == run.len() { padding } else { 0 },
                    private_key,
                });
            }
        }
    }
    spans
}

fn is_run_byte(b: u8) -> bool {
    is_base64_byte(b) || matches!(b, b'%' | b'.' | b'~' | b'+')
}

fn is_base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_')
}

/// `Some(private_key)` when the base64 text decodes to a credential anywhere in the decoded text.
fn decoded_credential(text: &[u8]) -> Option<bool> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for group in text.as_chunks::<4>().0 {
        let mut n = 0u32;
        for &c in group {
            n = (n << 6) | u32::from(base64_value(c)?);
        }
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    let decoded = match std::str::from_utf8(&out) {
        Ok(text) => text,
        Err(e) => std::str::from_utf8(&out[..e.valid_up_to()]).ok()?,
    };
    reiki_redact::scan(decoded)
        .first()
        .map(|span| span.private_key)
}

fn base64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

fn percent_decode(text: &[u8]) -> Vec<u8> {
    let hex = |b: u8| char::from(b).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let escaped = (text[i] == b'%')
            .then(|| Some(hex(*text.get(i + 1)?)? << 4 | hex(*text.get(i + 2)?)?))
            .flatten();
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(if text[i] == b'+' { b' ' } else { text[i] });
                i += 1;
            }
        }
    }
    out
}

/// Normalised field names whose string value is a credential.
const SECRET_FIELDS: &[&str] = &[
    "auth",
    "authorization",
    "bearer",
    "cookie",
    "setcookie",
    "credential",
    "credentials",
    "passwd",
    "pwd",
];

/// Normalised endings of field names whose string value is a credential.
const SECRET_FIELD_ENDINGS: &[&str] = &[
    "password",
    "secret",
    "apikey",
    "token",
    "secretkey",
    "privatekey",
    "accesskey",
];

/// The comparison form of a field or query parameter name: lowercase ASCII alphanumerics only.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn secret_field(name: &str) -> bool {
    let name = normalize(name);
    SECRET_FIELDS.contains(&name.as_str()) || SECRET_FIELD_ENDINGS.iter().any(|e| name.ends_with(e))
}

/// Streamed text fields that a model emits a piece at a time.
const DELTA_POINTERS: &[&str] = &[
    "/choices/0/delta/content",
    "/choices/0/delta/reasoning_content",
    "/delta/text",
    "/delta/partial_json",
];

/// Masks credentials that a stream spreads over several events, and, when the stream was cut, one
/// cut off at its end. Each text field is read as the whole it forms across the events.
pub fn stream_deltas(events: &mut [Value], cut: bool) {
    for pointer in DELTA_POINTERS {
        let pieces: Vec<(usize, String)> = events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| Some((i, e.pointer(pointer)?.as_str()?.to_owned())))
            .collect();
        if pieces.is_empty() || (pieces.len() < 2 && !cut) {
            continue;
        }
        let joined: String = pieces.iter().map(|(_, p)| p.as_str()).collect();
        let mut spans = spans_of(&joined);
        if cut {
            if let Some(start) = reiki_redact::partial_tail(&joined) {
                spans.push(Span {
                    start,
                    end: joined.len(),
                    private_key: false,
                });
                spans = merge(spans);
            }
        }
        if spans.is_empty() {
            continue;
        }
        let mut offset = 0;
        for (index, piece) in pieces {
            let (from, to) = (offset, offset + piece.len());
            offset = to;
            let mut text = String::with_capacity(piece.len());
            let mut cursor = from;
            for span in spans.iter().filter(|s| s.end > from && s.start < to) {
                text.push_str(&joined[cursor..span.start.max(from)]);
                if span.start >= from {
                    text.push_str(mark(span));
                }
                cursor = span.end.min(to);
            }
            text.push_str(&joined[cursor..to]);
            if text != piece {
                if let Some(slot) = events[index].pointer_mut(pointer) {
                    *slot = Value::String(text);
                }
            }
        }
    }
}

/// Headers whose whole value is a credential, whatever shape it takes.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-goog-api-key",
];

/// Query parameters whose whole value is a credential, after the name is percent-decoded and
/// normalised the way `secret_field` normalises a JSON field name.
const SECRET_PARAMS: &[&str] = &[
    "key",
    "apikey",
    "token",
    "accesstoken",
    "authtoken",
    "auth",
    "password",
    "passwd",
    "secret",
    "bearer",
    "signature",
];

/// Whether a query parameter carries a credential. The raw name is percent-decoded first, so
/// `api%2Dkey` and `api+key` match `api-key`, then normalised and matched whole or by one of the
/// known `secret_field` endings. Ordinary names such as `monkey` or `keyboard` are left alone.
fn secret_param(name: &str) -> bool {
    let decoded = percent_decode(name.as_bytes());
    let name = normalize(&String::from_utf8_lossy(&decoded));
    SECRET_PARAMS.contains(&name.as_str()) || SECRET_FIELD_ENDINGS.iter().any(|e| name.ends_with(e))
}

/// A header value, masked whole when the header carries credentials.
pub fn header<'a>(name: &str, value: &'a str) -> Cow<'a, str> {
    if SECRET_HEADERS.iter().any(|h| name.eq_ignore_ascii_case(h)) {
        return Cow::Borrowed(MASK);
    }
    secrets(value)
}

/// A request path with credential query parameters and credential-shaped segments masked.
pub fn path(path: &str) -> String {
    let (base, query) = match path.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (path, None),
    };
    let mut out = secrets(base).into_owned();
    if let Some(query) = query {
        let masked: Vec<String> = query
            .split('&')
            .map(|part| match part.split_once('=') {
                Some((name, _)) if secret_param(name) => {
                    format!("{name}={MASK}")
                }
                _ => secrets(part).into_owned(),
            })
            .collect();
        out.push('?');
        out.push_str(&masked.join("&"));
    }
    out
}

/// Masks every string value and object key in place, and the string under a field named like a
/// credential.
pub fn json(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(masked) = secrets(text) {
                *text = masked;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(json),
        Value::Object(map) => {
            if map.keys().any(|k| matches!(secrets(k), Cow::Owned(_))) {
                let old = std::mem::take(map);
                for (key, item) in old {
                    map.insert(secrets(&key).into_owned(), item);
                }
            }
            for (key, item) in map.iter_mut() {
                match item {
                    Value::String(text) if !text.is_empty() && secret_field(key) => {
                        *text = MASK.to_owned();
                    }
                    _ => json(item),
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> String {
        secrets(text).into_owned()
    }

    /// Fixtures are assembled at runtime so no token-shaped literal sits in the source.
    fn fake(prefix: &str, tail: &str) -> String {
        [prefix, tail].concat()
    }

    #[test]
    fn known_key_shapes_are_masked() {
        let tokens = [
            fake("sk-or-v1-", "0123456789abcdef0123456789abcdef"),
            fake("oc_sk_", "0123456789abcdef0123"),
            fake("ghp_", "0123456789abcdef0123456789abcdef0123"),
            fake("github_pat_", "0123456789abcdef0123456789abcdef"),
            fake("glpat-", "0123456789abcdef"),
            fake("AKIA", "0123456789ABCDEF"),
            fake("AIza", "0123456789abcdef0123456789abcdef"),
            fake("xoxb-", "0123456789-abcdefghij"),
        ];
        for token in tokens {
            assert_eq!(
                run(&format!("key {token} done")),
                "key [REDACTED] done",
                "{token}"
            );
        }
    }

    #[test]
    fn a_bearer_token_is_masked_but_the_word_stays() {
        assert_eq!(
            run("Authorization: Bearer abcdef0123456789abcdef"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(run("bearer abcdef0123456789abcdef"), "bearer [REDACTED]");
        assert_eq!(run("the Bearer of bad news"), "the Bearer of bad news");
    }

    #[test]
    fn ordinary_text_is_borrowed_unchanged() {
        for text in [
            "hello world",
            "task-abcdefghijklmnopqrstuvwxyz",
            "sk-short",
            "ask-abcdefghijklmnopqrstuvwxyz0123",
            "ghp_short",
            "",
            "naïve café — ok",
        ] {
            assert!(matches!(secrets(text), Cow::Borrowed(_)), "{text}");
        }
    }

    #[test]
    fn a_jwt_shaped_token_is_masked() {
        let jwt = [
            fake("eyJ", "hbGciOiJIUzI1NiJ9"),
            fake("eyJ", "zdWIiOiIxMjM0NTY3ODkwIn0"),
            "dBjftJeZ4CVPmB92K27uhbUJU1p1r".to_owned(),
        ]
        .join(".");
        assert_eq!(run(&format!("t={jwt};")), "t=[REDACTED];");
    }

    #[test]
    fn private_key_blocks_are_masked_whole() {
        let begin = fake("-----BEGIN RSA ", "PRIVATE KEY-----");
        let end = fake("-----END RSA ", "PRIVATE KEY-----");
        let pem = format!("a\n{begin}\nMIIabc\ndef\n{end}\nb");
        assert_eq!(run(&pem), "a\n[REDACTED PRIVATE KEY]\nb");
        let open = format!("x {}\nMIIabc", fake("-----BEGIN ", "PRIVATE KEY-----"));
        assert_eq!(run(&open), "x [REDACTED PRIVATE KEY]");
        let cert = "-----BEGIN CERTIFICATE-----\nMIIabc\n-----END CERTIFICATE-----";
        assert_eq!(run(cert), cert);
    }

    #[test]
    fn several_secrets_and_multibyte_neighbours() {
        let text = format!(
            "é {} ü {} ñ",
            fake("sk-", "0123456789abcdef0123456789"),
            fake("oc_sk_", "0123456789abcdef0123")
        );
        assert_eq!(run(&text), "é [REDACTED] ü [REDACTED] ñ");
    }

    #[test]
    fn json_strings_are_masked_even_after_an_escape() {
        let raw = r#"{"messages":[{"content":"line\nTOKEN end"}],"n":3}"#
            .replace("TOKEN", &fake("sk-", "0123456789abcdef0123456789"));
        let mut value: Value = serde_json::from_str(&raw).unwrap();
        json(&mut value);
        let text = value.to_string();
        assert!(!text.contains("sk-0123"), "{text}");
        assert!(text.contains("[REDACTED]"));
        assert!(text.contains("\"n\":3"));
    }

    #[test]
    fn credential_headers_are_masked_whole() {
        assert_eq!(header("Authorization", "Basic dXNlcjpwYXNz"), MASK);
        assert_eq!(header("cookie", "sid=abc"), MASK);
        assert_eq!(header("user-agent", "curl/8"), "curl/8");
    }

    #[test]
    fn query_credentials_and_path_tokens_are_masked() {
        assert_eq!(
            path("/zen/v1/models?key=abc&alt=sse&Token=x"),
            "/zen/v1/models?key=[REDACTED]&alt=sse&Token=[REDACTED]"
        );
        let token = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        assert_eq!(path(&format!("/x/{token}/y")), "/x/[REDACTED]/y");
        assert_eq!(path("/plain"), "/plain");
    }

    #[test]
    fn normalised_query_names_are_masked() {
        for name in [
            "key",
            "api_key",
            "api-key",
            "apikey",
            "ApiKey",
            "token",
            "access_token",
            "access-token",
            "accesstoken",
            "auth_token",
            "authtoken",
            "api_secret",
            "apisecret",
            "client_secret",
            "clientsecret",
            "password",
            "passwd",
            "secret",
            "bearer",
            "signature",
            "Auth",
        ] {
            assert_eq!(
                path(&format!("/x?{name}=value&keep=1")),
                format!("/x?{name}=[REDACTED]&keep=1"),
                "{name}"
            );
        }
    }

    #[test]
    fn ordinary_query_names_are_left_alone() {
        for name in [
            "monkey",
            "keyboard",
            "page_token_count",
            "tokens",
            "keys",
            "note",
        ] {
            assert_eq!(
                path(&format!("/x?{name}=value")),
                format!("/x?{name}=value"),
                "{name}"
            );
        }
    }

    #[test]
    fn percent_encoded_query_names_are_masked() {
        assert_eq!(path("/x?api%2Dkey=value"), "/x?api%2Dkey=[REDACTED]");
        assert_eq!(
            path("/x?access%5Ftoken=value"),
            "/x?access%5Ftoken=[REDACTED]"
        );
        assert_eq!(path("/x?api+key=value"), "/x?api+key=[REDACTED]");
        assert_eq!(path("/x?monkey=value"), "/x?monkey=value");
    }

    #[test]
    fn repeated_and_valueless_query_parameters() {
        assert_eq!(
            path("/x?key=a&key=b&note=c"),
            "/x?key=[REDACTED]&key=[REDACTED]&note=c"
        );
        assert_eq!(path("/x?token="), "/x?token=[REDACTED]");
        assert_eq!(path("/x?token"), "/x?token");
    }

    #[test]
    fn json_keys_are_masked_too() {
        let key = fake("sk-", "0123456789abcdef0123456789");
        let mut value: Value = serde_json::from_str(&format!("{{\"{key}\":1}}")).unwrap();
        json(&mut value);
        assert!(value.get("[REDACTED]").is_some(), "{value}");
    }

    fn b64(data: &str, url: bool) -> String {
        let alphabet: &[u8; 64] = if url {
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        } else {
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        };
        let mut out = String::new();
        for group in data.as_bytes().chunks(3) {
            let n = group
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..=group.len() {
                out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
            if !url {
                out.extend(std::iter::repeat_n('=', 3 - group.len()));
            }
        }
        out
    }

    #[test]
    fn newer_provider_prefixes_are_masked() {
        let tokens = [
            fake("sk_live_", "0123456789abcdef0123"),
            fake("hf_", "0123456789abcdefABCDEFghijklmnopqr"),
            fake("gsk_", "0123456789abcdefABCDEFghijklmnopqr"),
            fake("xai-", "0123456789abcdefABCDEFghijklmnopqr"),
            fake("npm_", "0123456789abcdefABCDEFghijklmnopqr"),
        ];
        for token in tokens {
            assert_eq!(
                run(&format!("key {token} done")),
                "key [REDACTED] done",
                "{token}"
            );
        }
    }

    #[test]
    fn a_base64_encoded_key_is_masked_in_both_alphabets() {
        let key = fake("sk-", "0123456789abcdef0123456789");
        for url in [false, true] {
            let encoded = b64(&key, url);
            assert_eq!(
                run(&format!("{{\"v\":\"{encoded}\"}}")),
                "{\"v\":\"[REDACTED]\"}",
                "{encoded}"
            );
        }
        let pem = format!(
            "{} {}\nMIIabc\n",
            fake("-----BEGIN ", "PRIVATE KEY-----"),
            "x"
        );
        assert_eq!(
            run(&format!("k={}", b64(&pem, false))),
            "k=[REDACTED PRIVATE KEY]"
        );
    }

    #[test]
    fn a_base64_value_with_a_credential_anywhere_is_masked_whole() {
        let key = fake("sk-", "0123456789abcdef0123456789");
        let cases = [
            format!("user:{key}"),
            format!(r#"{{"token":"{key}"}}"#),
            format!("prefix {key} tail"),
        ];
        for plain in cases {
            for url in [false, true] {
                let encoded = b64(&plain, url);
                assert_eq!(
                    run(&format!("v={encoded}")),
                    "v=[REDACTED]",
                    "{plain} -> {encoded}"
                );
            }
        }
        // A private key anywhere in the decoded text is still masked as a private key.
        let pem = format!(
            "prefix\n{}\nMIIabc\n",
            fake("-----BEGIN ", "PRIVATE KEY-----")
        );
        assert_eq!(
            run(&format!("k={}", b64(&pem, false))),
            "k=[REDACTED PRIVATE KEY]"
        );
        // A whole-value base64 key keeps being masked.
        assert_eq!(run(&format!("v={}", b64(&key, false))), "v=[REDACTED]");
        // A base64 value with no credential stays untouched.
        let plain = b64(
            "the quick brown fox jumps over the lazy dog, twice over",
            false,
        );
        assert!(matches!(secrets(&plain), Cow::Borrowed(_)), "{plain}");
    }

    #[test]
    fn a_percent_encoded_key_is_masked() {
        let key = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        let encoded: String = key.chars().map(|c| format!("%{:02X}", c as u32)).collect();
        assert_eq!(
            path(&format!("/x?note={encoded}&n=1")),
            "/x?note=[REDACTED]&n=1"
        );
        let bearer = format!("Bearer%20{}", fake("abcdefghij", "0123456789"));
        assert_eq!(run(&format!("a={bearer}&b=2")), "a=[REDACTED]&b=2");
    }

    #[test]
    fn ordinary_encoded_looking_text_is_left_alone() {
        let blob = b64(
            "the quick brown fox jumps over the lazy dog, twice over",
            false,
        );
        for text in [
            blob.as_str(),
            "https://example.com/a%20b/c%2Fd?q=hello%20world",
            "id=0123456789abcdef0123456789abcdef01234567",
            "AbstractSingletonProxyFactoryBeanImplementation",
        ] {
            assert!(matches!(secrets(text), Cow::Borrowed(_)), "{text}");
        }
    }

    #[test]
    fn fields_named_like_credentials_are_masked_by_name() {
        let mut value = serde_json::json!({
            "password": "hunter2",
            "Api-Key": "plain-value",
            "nested": {"client_secret": "abc", "refresh_token": "def"},
            "list": [{"authorization": "Basic dXNlcjpwYXNz"}],
            "max_tokens": 5,
            "tokens": "a b c",
            "schema": {"password": {"type": "string"}},
            "empty": "",
            "token": ""
        });
        json(&mut value);
        assert_eq!(value["password"], MASK);
        assert_eq!(value["Api-Key"], MASK);
        assert_eq!(value["nested"]["client_secret"], MASK);
        assert_eq!(value["nested"]["refresh_token"], MASK);
        assert_eq!(value["list"][0]["authorization"], MASK);
        assert_eq!(value["max_tokens"], 5);
        assert_eq!(value["tokens"], "a b c");
        assert_eq!(value["schema"]["password"]["type"], "string");
        assert_eq!(value["token"], "");
    }

    #[test]
    fn a_credential_cut_at_the_end_is_masked_only_when_the_text_was_cut() {
        let cut = format!("hello {}", fake("sk-", "0123456"));
        assert_eq!(secrets_cut(&cut, true), "hello [REDACTED]");
        assert_eq!(secrets_cut(&cut, false), cut.as_str());
        let bearer = "x Bearer abcdef01";
        assert_eq!(secrets_cut(bearer, true), "x [REDACTED]");
        assert!(matches!(secrets_cut("plain words", true), Cow::Borrowed(_)));
    }

    fn delta(content: &str) -> Value {
        serde_json::json!({"choices": [{"delta": {"content": content}}]})
    }

    #[test]
    fn a_secret_split_across_stream_deltas_is_masked_in_every_piece() {
        let key = fake("sk-", "0123456789abcdef0123456789");
        let (a, rest) = key.split_at(5);
        let (b, c) = rest.split_at(9);
        let mut events = vec![
            delta(&format!("use {a}")),
            delta(b),
            delta(&format!("{c} now")),
        ];
        stream_deltas(&mut events, false);
        let text: String = events
            .iter()
            .map(|e| e["choices"][0]["delta"]["content"].as_str().unwrap())
            .collect();
        assert_eq!(text, "use [REDACTED] now");
        assert!(
            !events.iter().any(|e| e.to_string().contains("sk-0")),
            "{events:?}"
        );
    }

    #[test]
    fn a_stream_cut_inside_a_secret_masks_the_tail_of_the_text() {
        let mut events = vec![delta("see "), delta(&fake("sk-", "0123"))];
        stream_deltas(&mut events, true);
        assert_eq!(events[1]["choices"][0]["delta"]["content"], "[REDACTED]");
        let mut events = vec![delta("see "), delta(&fake("sk-", "0123"))];
        stream_deltas(&mut events, false);
        assert_eq!(events[1]["choices"][0]["delta"]["content"], "sk-0123");
    }
}
