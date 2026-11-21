//! Bounded parser entry points used only by the cargo-fuzz targets in `fuzz/`.
//!
//! This module is compiled only with the `fuzzing` feature. It exposes internal parsers that the
//! fuzz targets need without widening their visibility in normal builds. Nothing here is part of
//! the proxy's runtime behaviour: every function just forwards to code the proxy already uses.

use axum::http::HeaderMap;

#[cfg(feature = "peers")]
use crate::peers::client::{SseItem, SseParser};

/// A count and byte total from one or more `SseParser::feed` calls.
#[cfg(feature = "peers")]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SseSummary {
    pub items: usize,
    pub bytes: usize,
}

/// Feeds already-split chunks through the same SSE parser the peer watcher uses.
#[cfg(feature = "peers")]
pub fn sse_feed(chunks: &[&[u8]], cap: usize) -> Result<SseSummary, String> {
    let mut parser = SseParser::new(cap);
    let mut summary = SseSummary::default();
    for chunk in chunks {
        for item in parser.feed(chunk)? {
            summary.items += 1;
            if let SseItem::Event { event, data } = item {
                summary.bytes += event.len() + data.len();
            }
        }
    }
    Ok(summary)
}

/// The per-feed item cap enforced by the peer watcher's SSE parser.
#[cfg(feature = "peers")]
pub const MAX_SSE_ITEMS_PER_FEED: usize = crate::peers::client::MAX_SSE_ITEMS_PER_FEED;

/// JSON property matcher used when classifying a 400 response.
pub fn has_json_pair(body: &str, field: &str, value: &str) -> bool {
    crate::classify::has_json_pair(body, field, value)
}

/// Repeated percent-decoding check applied to a routing path.
pub fn has_path_trick(path: &str) -> bool {
    crate::proxy::has_path_trick(path)
}

/// One percent-decoding pass applied to a routing path.
pub fn decode_path_layer(path: &str) -> Option<String> {
    crate::proxy::decode_path_layer(path)
}

/// The upstream and remainder of a routing path, both slices of the input.
pub fn split_upstream(path: &str) -> Option<(String, String)> {
    crate::proxy::split_upstream(path)
        .map(|(upstream, rest)| (upstream.to_owned(), rest.to_owned()))
}

/// Strict percent-decoding of one admin query value.
pub fn decode_query_value(value: &str) -> Option<String> {
    crate::admin::decode_query_value(value)
}

/// Strict parsing of the `?key=` admin query.
pub fn parse_key_query(query: &str) -> Result<String, &'static str> {
    crate::admin::parse_key_query(query)
}

/// Strict parsing of the `?dry_run=` reload query.
pub fn parse_dry_run(query: &str) -> Result<bool, ()> {
    crate::admin::parse_dry_run(query)
}

/// Canonical host and port parsing shared by tunnel rules and CONNECT targets.
pub fn split_host_port(text: &str, default_port: Option<u16>) -> Option<(String, u16)> {
    crate::tunnel::split_host_port(text, default_port)
}

/// Bearer credential parser for an `Authorization` header value.
pub fn bearer_token(value: &str) -> Option<&str> {
    crate::auth::bearer_token(value)
}

/// Pure validation of a key-file's contents and mode, without touching the filesystem.
pub fn key_file_verdict(
    name: &str,
    raw: &str,
    mode: Option<u32>,
    strict: bool,
) -> Result<String, crate::proxy::LoadError> {
    crate::proxy::key_file_verdict(name, raw, mode, strict)
}

/// Provider `x-ratelimit-*` duration parser.
pub fn parse_duration(value: &str) -> Option<std::time::Duration> {
    crate::shaper::headers::parse_duration(value)
}

/// Deterministic-clock variant of the rate-limit header observer.
pub fn observe_at(headers: &HeaderMap, local_now: i64) -> crate::shaper::headers::Observed {
    crate::shaper::headers::observe_at(headers, local_now)
}
