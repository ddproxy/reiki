use std::borrow::Cow;

use serde_json::Value;

const MASK: &str = "[REDACTED]";

/// Token prefixes with the fewest token characters that must follow, so ordinary words are left alone.
const PREFIXES: &[(&str, usize)] = &[
    ("sk-", 20),
    ("oc_sk_", 16),
    ("ghp_", 30),
    ("gho_", 30),
    ("ghu_", 30),
    ("ghs_", 30),
    ("ghr_", 30),
    ("github_pat_", 30),
    ("glpat-", 16),
    ("AKIA", 16),
    ("ASIA", 16),
    ("AIza", 30),
    ("xoxb-", 20),
    ("xoxp-", 20),
    ("xoxa-", 20),
    ("xoxs-", 20),
    ("eyJ", 40),
];

const BEARER: &[u8] = b"bearer ";
const BEARER_MIN: usize = 16;

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn token_len(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| is_token(**b)).count()
}

/// Masks credential-shaped substrings; borrows the input when nothing matched.
pub fn secrets(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;

    let replace =
        |out: &mut Option<String>, from: usize, to: usize, copied: &mut usize, mask: &str| {
            let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
            buf.push_str(&text[*copied..from]);
            buf.push_str(mask);
            *copied = to;
        };

    while i < bytes.len() {
        let boundary = i == 0 || !is_word(bytes[i - 1]);
        if !boundary {
            i += 1;
            continue;
        }

        if bytes[i..].starts_with(b"-----BEGIN ") {
            if let Some(end) = private_key_end(text, i) {
                replace(&mut out, i, end, &mut copied, "[REDACTED PRIVATE KEY]");
                i = end;
                continue;
            }
        }

        if bytes.len() - i >= BEARER.len()
            && bytes[i..i + BEARER.len()].eq_ignore_ascii_case(BEARER)
        {
            let start = i + BEARER.len();
            let len = token_len(&bytes[start..]);
            if len >= BEARER_MIN {
                replace(&mut out, start, start + len, &mut copied, MASK);
                i = start + len;
                continue;
            }
        }

        let mut matched = false;
        for (prefix, min) in PREFIXES {
            if bytes[i..].starts_with(prefix.as_bytes()) {
                let after = i + prefix.len();
                let len = token_len(&bytes[after..]);
                if len >= *min {
                    replace(&mut out, i, after + len, &mut copied, MASK);
                    i = after + len;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            i += 1;
        }
    }

    match out {
        None => Cow::Borrowed(text),
        Some(mut buf) => {
            buf.push_str(&text[copied..]);
            Cow::Owned(buf)
        }
    }
}

/// End of the private-key block at `start`, to the end of text if unclosed; `None` for other PEM blocks.
fn private_key_end(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let header_end = rest[5..].find("-----")? + 5;
    if !rest[..header_end].contains("PRIVATE KEY") {
        return None;
    }
    match rest[header_end..].find("-----END ") {
        Some(offset) => {
            let footer = header_end + offset + "-----END ".len();
            let close = rest[footer..].find("-----").map(|p| footer + p + 5);
            Some(start + close.unwrap_or(rest.len()))
        }
        None => Some(text.len()),
    }
}

/// Masks every string value in place.
pub fn json(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(masked) = secrets(text) {
                *text = masked;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(json),
        Value::Object(map) => map.values_mut().for_each(json),
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
}
