//! Types for plugins compiled into Reiki.
//!
//! A plugin hooks request and response bodies as they pass through the relay.
//! Hooks are synchronous and run on the request's task, so keep them CPU-bound
//! and free of blocking I/O. One plugin instance serves many concurrent
//! requests and keeps per-request data in the state returned by
//! [`Plugin::new_state`].

use std::any::Any;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};

/// A fresh 16-hex-digit request id, unique enough to follow one request through logs.
pub fn new_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    hasher.write_u128(nanos);
    format!("{:016x}", hasher.finish())
}

/// Hooks into the request and response path. `Send + Sync` so a single
/// instance can serve concurrent requests.
pub trait Plugin: Send + Sync {
    /// Stable name used in config and logs.
    fn name(&self) -> &str;

    /// Builds the per-request state that every hook for this request receives.
    fn new_state(&self) -> Box<dyn Any + Send> {
        Box::new(())
    }

    /// Called once before the relay picks a key, so failover replays all send
    /// the same transformed request. Returning an error aborts the request.
    fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Result<(), PluginError> {
        Ok(())
    }

    /// Called after each upstream attempt, in order, including attempts that fail
    /// over to the next key. The default does nothing.
    fn on_attempt(&self, _ctx: &mut AttemptCtx<'_>) {}

    /// Called when the upstream response head arrives, before any body bytes.
    fn on_response_head(&self, _ctx: &mut ResponseHeadCtx<'_>) {}

    /// Called for each response body chunk. `is_last` marks a final, possibly
    /// empty, call after which the plugin must release anything it held back.
    fn on_response_chunk(&self, _ctx: &mut ChunkCtx<'_>) {}
}

/// A hook declined or failed. The relay decides what to do with it.
#[derive(Debug)]
#[non_exhaustive]
pub enum PluginError {
    /// Return the status and message to the client without calling upstream.
    Reject(StatusCode, String),
    /// Report that the plugin did not apply. Non-fatal; the relay either logs
    /// and continues or rejects, depending on the plugin's `on_error` setting.
    Skip(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reject(status, message) => write!(f, "rejected with {status}: {message}"),
            Self::Skip(message) => write!(f, "skipped: {message}"),
        }
    }
}

impl std::error::Error for PluginError {}

/// The request before it is sent upstream. `state` is this request's value from
/// [`Plugin::new_state`], downcast it with `Any::downcast_mut`.
#[non_exhaustive]
pub struct RequestCtx<'a> {
    /// The id Reiki gives this request in its own logs; empty when the host set none.
    pub id: &'a str,
    pub stream: &'a str,
    pub upstream: &'a str,
    pub method: &'a Method,
    /// The request path as the client sent it, without the query string.
    pub path: &'a str,
    pub headers: &'a mut HeaderMap,
    pub body: &'a mut Bytes,
    pub state: &'a mut (dyn Any + Send),
}

/// One upstream attempt. `key` is the name of the key in the config, never its
/// secret value.
#[non_exhaustive]
pub struct AttemptCtx<'a> {
    pub key: &'a str,
    /// The upstream status, or `None` when no response arrived (a connection
    /// error or a timeout).
    pub status: Option<u16>,
    /// The start of the upstream's error body; empty for a success, which streams
    /// through [`Plugin::on_response_chunk`], and when no response arrived.
    pub body: &'a [u8],
    pub state: &'a mut (dyn Any + Send),
}

/// The upstream response head. A plugin that changes the response body length
/// must remove `content-length` here so the relay does not forward a stale one.
#[non_exhaustive]
pub struct ResponseHeadCtx<'a> {
    /// The name of the key in the config that produced this response.
    pub key: &'a str,
    pub status: &'a mut StatusCode,
    pub headers: &'a mut HeaderMap,
    pub state: &'a mut (dyn Any + Send),
}

/// One response body chunk. Append `chunk` to your own buffer, write back the
/// bytes that are safe to release, and release the rest when `is_last` is true.
/// Holding bytes lets a placeholder split across chunks be reassembled before
/// it is replaced.
#[non_exhaustive]
pub struct ChunkCtx<'a> {
    pub chunk: &'a mut Bytes,
    pub is_last: bool,
    pub state: &'a mut (dyn Any + Send),
}

impl<'a> RequestCtx<'a> {
    pub fn new(
        stream: &'a str,
        upstream: &'a str,
        method: &'a Method,
        path: &'a str,
        headers: &'a mut HeaderMap,
        body: &'a mut Bytes,
        state: &'a mut (dyn Any + Send),
    ) -> Self {
        Self {
            id: "",
            stream,
            upstream,
            method,
            path,
            headers,
            body,
            state,
        }
    }

    /// Sets the request id the host logs this request under.
    #[must_use]
    pub fn with_id(mut self, id: &'a str) -> Self {
        self.id = id;
        self
    }
}

impl<'a> AttemptCtx<'a> {
    pub fn new(
        key: &'a str,
        status: Option<u16>,
        body: &'a [u8],
        state: &'a mut (dyn Any + Send),
    ) -> Self {
        Self {
            key,
            status,
            body,
            state,
        }
    }
}

impl<'a> ResponseHeadCtx<'a> {
    pub fn new(
        key: &'a str,
        status: &'a mut StatusCode,
        headers: &'a mut HeaderMap,
        state: &'a mut (dyn Any + Send),
    ) -> Self {
        Self {
            key,
            status,
            headers,
            state,
        }
    }
}

impl<'a> ChunkCtx<'a> {
    pub fn new(chunk: &'a mut Bytes, is_last: bool, state: &'a mut (dyn Any + Send)) -> Self {
        Self {
            chunk,
            is_last,
            state,
        }
    }
}
