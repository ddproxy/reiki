use std::borrow::Cow;

use serde_json::Value;

const MASK: &str = "[REDACTED]";
const PRIVATE_KEY_MASK: &str = "[REDACTED PRIVATE KEY]";

/// Masks credential-shaped substrings; borrows the input when nothing matched.
pub fn secrets(text: &str) -> Cow<'_, str> {
    let spans = reiki_redact::scan(text);
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for span in spans {
        out.push_str(&text[copied..span.start]);
        out.push_str(if span.private_key {
            PRIVATE_KEY_MASK
        } else {
            MASK
        });
        copied = span.end;
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
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
