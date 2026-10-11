use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub const CREDENTIAL_HEADERS: [&str; 2] = ["x-api-key", "x-goog-api-key"];

pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let value = value.trim();
        if value.len() > 7 && value[..7].eq_ignore_ascii_case("bearer ") {
            return Some(value[7..].trim().to_owned());
        }
    }
    CREDENTIAL_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn hashes_match_the_known_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn constant_time_eq_compares_values() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }

    #[test]
    fn token_comes_from_bearer_or_api_key() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer tok-1"));
        assert_eq!(extract_token(&h).as_deref(), Some("tok-1"));
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("bearer   tok-2 "));
        assert_eq!(extract_token(&h).as_deref(), Some("tok-2"));
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("tok-3"));
        assert_eq!(extract_token(&h).as_deref(), Some("tok-3"));
        let mut h = HeaderMap::new();
        h.insert("x-goog-api-key", HeaderValue::from_static("tok-4"));
        assert_eq!(extract_token(&h).as_deref(), Some("tok-4"));
        assert_eq!(extract_token(&HeaderMap::new()), None);
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Basic abc"));
        assert_eq!(extract_token(&h), None);
    }
}
