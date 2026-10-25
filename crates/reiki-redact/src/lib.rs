//! Finds credential-shaped substrings in text.
//!
//! The matches are heuristics for common key formats, not a guarantee that every
//! secret is found. Callers decide what to do with the spans: mask them, swap them
//! for placeholders, or leave them out of a log.

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

/// A credential-shaped range of the scanned text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    /// A PEM private key block, which no caller should ever need to restore.
    pub private_key: bool,
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

fn token_len(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| is_token(**b)).count()
}

/// Every credential-shaped range in `text`, in order and never overlapping.
pub fn scan(text: &str) -> Vec<Span> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let boundary = i == 0 || !is_word(bytes[i - 1]);
        if !boundary {
            i += 1;
            continue;
        }

        if bytes[i..].starts_with(b"-----BEGIN ") {
            if let Some(end) = private_key_end(text, i) {
                spans.push(Span {
                    start: i,
                    end,
                    private_key: true,
                });
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
                spans.push(Span {
                    start,
                    end: start + len,
                    private_key: false,
                });
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
                    spans.push(Span {
                        start: i,
                        end: after + len,
                        private_key: false,
                    });
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
    spans
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures are assembled at runtime so no token-shaped literal sits in the source.
    fn fake(prefix: &str, tail: &str) -> String {
        [prefix, tail].concat()
    }

    fn found(text: &str) -> Vec<&str> {
        scan(text).iter().map(|s| &text[s.start..s.end]).collect()
    }

    #[test]
    fn a_key_is_found_with_its_exact_range() {
        let key = fake("ghp_", "0123456789abcdef0123456789abcdef0123");
        let text = format!("token {key} done");
        assert_eq!(found(&text), vec![key.as_str()]);
    }

    #[test]
    fn a_bearer_span_covers_only_the_token() {
        let token = fake("abcdefghij", "0123456789");
        let text = format!("Authorization: Bearer {token}");
        assert_eq!(found(&text), vec![token.as_str()]);
    }

    #[test]
    fn a_private_key_block_is_one_span_marked_as_such() {
        let text = "x -----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY----- y";
        let spans = scan(text);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].private_key);
        assert!(text[spans[0].start..spans[0].end].ends_with("PRIVATE KEY-----"));
    }

    #[test]
    fn ordinary_words_and_short_tokens_are_left_alone() {
        assert!(scan("the sk-short and a plain sentence").is_empty());
        assert!(scan("public certificate -----BEGIN CERTIFICATE----- body").is_empty());
    }

    #[test]
    fn spans_do_not_overlap_and_are_ordered() {
        let a = fake("AKIA", "0123456789ABCDEF");
        let b = fake("xoxb-", "0123456789-abcdefghij");
        let text = format!("{a} {b}");
        let spans = scan(&text);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].end <= spans[1].start);
    }
}
