//! Types for plugins compiled into Reiki.
//!
//! A plugin hooks request and response bodies as they pass through the relay.
//! Hooks are synchronous and run on the request's task, so keep them CPU-bound
//! and free of blocking I/O. One plugin instance serves many concurrent
//! requests and keeps per-request data in the state returned by
//! [`Plugin::new_state`].

use std::any::Any;
use std::fmt;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};

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

    /// Called when the upstream response head arrives, before any body bytes.
    fn on_response_head(&self, _ctx: &mut ResponseHeadCtx<'_>) {}

    /// Called for each response body chunk. `is_last` marks a final, possibly
    /// empty, call after which the plugin must release anything it held back.
    fn on_response_chunk(&self, _ctx: &mut ChunkCtx<'_>) {}
}

/// A hook declined or failed. The relay decides what to do with it.
#[derive(Debug)]
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
pub struct RequestCtx<'a> {
    pub stream: &'a str,
    pub upstream: &'a str,
    pub method: &'a Method,
    pub headers: &'a mut HeaderMap,
    pub body: &'a mut Bytes,
    pub state: &'a mut (dyn Any + Send),
}

/// The upstream response head. A plugin that changes the response body length
/// must remove `content-length` here so the relay does not forward a stale one.
pub struct ResponseHeadCtx<'a> {
    pub status: &'a mut StatusCode,
    pub headers: &'a mut HeaderMap,
    pub state: &'a mut (dyn Any + Send),
}

/// One response body chunk. Append `chunk` to your own buffer, write back the
/// bytes that are safe to release, and release the rest when `is_last` is true.
/// Holding bytes lets a placeholder split across chunks be reassembled before
/// it is replaced.
pub struct ChunkCtx<'a> {
    pub chunk: &'a mut Bytes,
    pub is_last: bool,
    pub state: &'a mut (dyn Any + Send),
}
