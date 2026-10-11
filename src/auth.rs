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

pub fn extract_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    token_from_headers(headers).or_else(|| query.and_then(token_from_query))
}

fn token_from_headers(headers: &HeaderMap) -> Option<String> {
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

fn param_name(part: &str) -> &str {
    part.split_once('=').map_or(part, |(name, _)| name)
}

fn token_from_query(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            (name == "key" && !value.is_empty()).then(|| value.to_owned())
        })
        .next()
}

// Drops every `key` parameter. Other parameters keep their order and their raw
// percent-encoding, since re-encoding them could change what the upstream receives.
pub fn strip_query_key(query: &str) -> Option<String> {
    let kept = query
        .split('&')
        .filter(|part| param_name(part) != "key")
        .collect::<Vec<_>>()
        .join("&");
    (!kept.is_empty()).then_some(kept)
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
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-1"));
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("bearer   tok-2 "));
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-2"));
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("tok-3"));
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-3"));
        let mut h = HeaderMap::new();
        h.insert("x-goog-api-key", HeaderValue::from_static("tok-4"));
        assert_eq!(extract_token(&h, None).as_deref(), Some("tok-4"));
        assert_eq!(extract_token(&HeaderMap::new(), None), None);
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Basic abc"));
        assert_eq!(extract_token(&h, None), None);
    }

    #[test]
    fn query_key_is_the_last_token_source() {
        assert_eq!(
            extract_token(&HeaderMap::new(), Some("key=query-tok")).as_deref(),
            Some("query-tok")
        );
        assert_eq!(
            extract_token(&HeaderMap::new(), Some("a=1&key=query-tok&b=2")).as_deref(),
            Some("query-tok")
        );
        assert!(extract_token(&HeaderMap::new(), Some("a=1")).is_none());
        assert!(extract_token(&HeaderMap::new(), Some("key=")).is_none());
        assert!(extract_token(&HeaderMap::new(), Some("keyless=1")).is_none());

        let mut h = HeaderMap::new();
        h.insert("x-goog-api-key", HeaderValue::from_static("header-tok"));
        assert_eq!(
            extract_token(&h, Some("key=query-tok")).as_deref(),
            Some("header-tok")
        );
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static("Bearer bearer-tok"),
        );
        h.insert("x-api-key", HeaderValue::from_static("api-tok"));
        assert_eq!(
            extract_token(&h, Some("key=query-tok")).as_deref(),
            Some("bearer-tok")
        );
    }

    #[test]
    fn query_rewrite_drops_only_the_key_parameter() {
        assert_eq!(strip_query_key("key=tok").as_deref(), None);
        assert_eq!(strip_query_key("").as_deref(), None);
        assert_eq!(
            strip_query_key("key=tok&alt=sse").as_deref(),
            Some("alt=sse")
        );
        assert_eq!(
            strip_query_key("alt=sse&key=tok").as_deref(),
            Some("alt=sse")
        );
        assert_eq!(
            strip_query_key("a=1&key=tok&b=2").as_deref(),
            Some("a=1&b=2")
        );
        assert_eq!(
            strip_query_key("key=one&key=two&a=1").as_deref(),
            Some("a=1")
        );
        assert_eq!(
            strip_query_key("apikey=1&keyless=2").as_deref(),
            Some("apikey=1&keyless=2")
        );
        assert_eq!(
            strip_query_key("q=a%20b&key=tok&r=%2F").as_deref(),
            Some("q=a%20b&r=%2F")
        );
    }
}
