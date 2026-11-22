//! Types for plugins compiled into Reiki.
//!
//! A plugin hooks request and response bodies as they pass through the relay.
//! Hooks are synchronous and run on the request's task, so keep them CPU-bound
//! and free of blocking I/O. One plugin instance serves many concurrent
//! requests and keeps per-request data in the state returned by
//! [`Plugin::new_state`].

mod timefmt;

pub use timefmt::{civil_from_days, days_from_civil, format_utc};

use std::any::Any;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;

/// Keys every plugin table accepts. The host reads them, so a plugin must not reject them as unknown.
pub const HOST_KEYS: &[&str] = &["enabled", "on_error", "dry_run", "streams"];

/// A fresh 32-hex-digit request id: a per-process nonce followed by a counter.
pub fn new_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let nonce = *NONCE.get_or_init(|| {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        hasher.write_u128(nanos);
        hasher.finish()
    });
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nonce:016x}{counter:016x}")
}

/// Host facts a plugin may need that are not part of its config table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginHost {
    /// Private, append-only path the plugin may use for its own counters, derived
    /// from the config path by the host. No plugin may choose an arbitrary path.
    pub state_path: Option<PathBuf>,
}

impl PluginHost {
    /// Builds host facts with a private state path. No plugin may choose its own path.
    pub fn with_state_path(path: PathBuf) -> Self {
        Self {
            state_path: Some(path),
        }
    }
}

/// Hooks into the request and response path. `Send + Sync` so a single
/// instance can serve concurrent requests.
pub trait Plugin: Send + Sync {
    /// Stable name used in config and logs.
    fn name(&self) -> &str;

    /// Receives host-supplied facts after construction and before the chain is used.
    fn configure(&mut self, _host: &PluginHost) {}

    /// True when a failure of this plugin must stop the request, because letting it through
    /// would send data the plugin was meant to mask. The host rejects a request that this
    /// plugin fails unless the operator sets `on_error = "pass_through"` explicitly.
    fn fails_closed(&self) -> bool {
        false
    }

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

    /// Called once when the process is stopping, after requests in flight have finished or the
    /// drain time ran out. A plugin that buffers output (a log writer) writes it out here and
    /// returns; it must not wait for long.
    fn on_shutdown(&self) {}

    /// False when a changed config table cannot be applied to a running instance, because the
    /// plugin holds a file, a socket or counters that a rebuild would lose. A reload then keeps the
    /// running instance and reports that a restart is needed.
    fn live_reload(&self) -> bool {
        true
    }

    /// Files whose contents shape this plugin's behaviour, such as a terms list. A reload builds
    /// the plugin again when one of them changed, even if its config table did not.
    fn watched_files(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    /// True when the plugin needs the response head and body chunks, even if it does not change them.
    /// Defaults to [`Plugin::uses_bodies`] so existing plugins keep their behaviour.
    fn needs_response_hooks(&self) -> bool {
        self.uses_bodies()
    }

    /// False for a plugin that never changes a body, so its presence keeps `content-length` on responses.
    /// A plugin that only observes bodies returns `false` here and `true` from
    /// [`Plugin::needs_response_hooks`].
    fn uses_bodies(&self) -> bool {
        true
    }

    /// Called for relay-level facts, not tied to one request's state.
    fn on_event(&self, _event: &Event<'_>) {}

    /// Paths this plugin answers on the admin listener; two plugins claiming one path is a startup error.
    fn admin_paths(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Answers a GET for one of [`Plugin::admin_paths`], after the caller is authenticated.
    fn on_admin(&self, _ctx: &AdminCtx<'_>) -> Option<AdminReply> {
        None
    }

    /// A JSON object merged into `/v1/status`. Return `None` when the plugin has
    /// no counters to publish; the returned value must be an object.
    fn status_fragment(&self) -> Option<Value> {
        None
    }

    /// One monotonic counter published in `/v1/health` as `name`. The health
    /// document is shared with peers, so the value must not carry request data.
    fn health_gauge(&self) -> Option<(&'static str, u64)> {
        None
    }

    /// A refusal this plugin wants at admission time. The host converts it to the
    /// gateway's own refusal path, so it cannot bypass stream limits or produce a
    /// different response shape. Messages must be static: no request data, prices
    /// or token counts can reach a client or a log through this hook.
    fn admission_refusal(&self, _stream: &str, _now: SystemTime) -> Option<AdmissionRefusal> {
        None
    }

    /// Extra Prometheus text appended to the metrics plugin's answer. The host
    /// supplies it to `/v1/metrics`; no plugin answers on the metrics path itself.
    /// Implementations must use bounded label values and never include secrets.
    fn metrics_text(&self) -> String {
        String::new()
    }

    /// A JSON object merged into the authenticated stream's own `/v1/usage`
    /// answer. The host calls this only for the stream the token belongs to.
    fn usage_cost_fragment(&self, _stream: &str) -> Option<Value> {
        None
    }
}

/// Why a key left rotation, as a stable label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureKind {
    RateLimited,
    QuotaExhausted,
    AccessDisabled,
    Unauthorized,
    Transient,
    RequestError,
}

impl FailureKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::QuotaExhausted => "quota_exhausted",
            Self::AccessDisabled => "access_disabled",
            Self::Unauthorized => "unauthorized",
            Self::Transient => "transient",
            Self::RequestError => "request_error",
        }
    }
}

/// Whether a cooldown applies to the whole key or only to one route and model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownScope {
    Key,
    Model,
}

impl CooldownScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Model => "model",
        }
    }
}

/// Why the multi-user gateway refused a request. Fixed labels only; safe as metric values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum GatewayRejectReason {
    Upstream,
    Model,
    ModelUnknown,
    Rate,
    Tokens,
    Concurrent,
    QuotaRequests,
    QuotaTokens,
    Fairness,
    StoreClosed,
    CostBudget,
}

impl GatewayRejectReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Model => "model",
            Self::ModelUnknown => "model_unknown",
            Self::Rate => "rate",
            Self::Tokens => "tokens",
            Self::Concurrent => "concurrent",
            Self::QuotaRequests => "quota_requests",
            Self::QuotaTokens => "quota_tokens",
            Self::Fairness => "fairness",
            Self::StoreClosed => "store_closed",
            Self::CostBudget => "cost_budget",
        }
    }
}

/// A plugin's request for admission refusal. Both strings are static so a plugin
/// cannot put request data, prices or credentials into a client-visible message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionRefusal {
    /// Stable error kind, for example `budget_monthly`.
    pub kind: &'static str,
    /// Public message. It may name a budget but never an amount or a secret.
    pub message: &'static str,
}

/// Maps an HTTP status to one of the four bounded classes used by events and metrics.
pub fn status_class(status: u16) -> &'static str {
    match status / 100 {
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    }
}

/// Something the relay did; key names come from the config, never secrets.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Event<'a> {
    /// Once per request: the client-visible status and the serving upstream (the fallback, after a failover).
    RequestDone {
        stream: &'a str,
        upstream: &'a str,
        status: u16,
    },
    /// A request moved from a failed key to the next.
    Failover {
        stream: &'a str,
        key: &'a str,
        kind: FailureKind,
    },
    /// A key or one of its route-and-model combinations went on cooldown.
    Cooldown {
        key: &'a str,
        kind: FailureKind,
        scope: CooldownScope,
    },
    /// A stream's rate limit turned a request away before any key was picked.
    RateLimited { stream: &'a str },
    /// The gateway refused a request for one of its own configured stream limits or allowlists.
    /// The reason is a fixed label, never request data.
    GatewayRejected {
        stream: &'a str,
        reason: GatewayRejectReason,
    },
    /// Counter state was unavailable; in `open` mode the request is still served.
    GatewayStoreError { stream: &'a str },
    /// Final token usage of a response, emitted when its body ends. Separate from `RequestDone`,
    /// which fires as soon as response headers are available. `model` is empty when the request
    /// did not declare one; `key` is the operator's key name, never a credential. Counts are the
    /// measured values read from the response body, not the admission estimate.
    Usage {
        stream: &'a str,
        upstream: &'a str,
        model: &'a str,
        key: &'a str,
        tokens: u64,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        status_class: &'static str,
    },
    /// A plugin stopped a request: it returned `Reject`, or it skipped under `on_error = "reject"`.
    PluginRejected { stream: &'a str, plugin: &'a str },
    /// A passthrough response was classified for observation. The account is an operator label;
    /// neither it nor the class carries a credential or any response text.
    Observed {
        upstream: &'a str,
        account: &'a str,
        status_class: &'a str,
    },
    /// A detection plugin found identifiers in a request body: how many of one kind, and whether
    /// the request was blocked for it. The event never carries the matched text.
    Finding {
        /// The id of the request the finding is in, as it appears in Reiki's logs.
        id: &'a str,
        stream: &'a str,
        plugin: &'a str,
        kind: &'a str,
        /// A short label for one name or rule within the kind, such as a tool name. The host
        /// sanitises it before a listener sees it; see [`RequestCtx::report_detail`].
        detail: Option<&'a str>,
        count: u64,
        blocked: bool,
    },
}

/// Key counts when an admin path is asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyCounts {
    pub available: u64,
    pub cooling: u64,
    pub out_of_rotation: u64,
}

/// One upstream's attempts and tokens over the last hour, sampled for an admin request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamUsage {
    pub upstream: String,
    pub requests_1h: u64,
    pub limited_1h: u64,
    pub tokens_1h: u64,
    /// Responses in the last hour whose token usage could not be read because the body passed
    /// the extraction caps. A count only; no body text is kept.
    pub unaccounted_1h: u64,
}

/// A passthrough account's remaining observed cooldown, sampled for an admin request. The
/// account is an operator label and is never included in a metric, where it would multiply
/// cardinality; it is still carried here for plugins that need to group a local report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedCooldown {
    pub upstream: String,
    pub account: String,
    pub class: String,
    pub seconds_remaining: u64,
}

/// An admin request for one of a plugin's paths.
#[derive(Debug)]
#[non_exhaustive]
pub struct AdminCtx<'a> {
    pub path: &'a str,
    /// Raw query string without the leading `?`. Plugins that need filters parse it themselves.
    pub query: &'a str,
    pub keys: KeyCounts,
    /// Per-upstream usage, sampled for this request; `None` when the operator hides usage.
    pub usage: Option<Vec<UpstreamUsage>>,
    /// Remaining cooldowns of observed passthrough accounts, sampled for this request.
    pub observed: Vec<ObservedCooldown>,
    /// Extra Prometheus text supplied by the host, for features outside the plugin set.
    /// Empty for ordinary callers and tests.
    pub extra_metrics: String,
}

impl<'a> AdminCtx<'a> {
    pub fn new(path: &'a str, keys: KeyCounts) -> Self {
        Self {
            path,
            query: "",
            keys,
            usage: None,
            observed: Vec::new(),
            extra_metrics: String::new(),
        }
    }

    pub fn with_query(mut self, query: &'a str) -> Self {
        self.query = query;
        self
    }

    pub fn with_usage(mut self, usage: Option<Vec<UpstreamUsage>>) -> Self {
        self.usage = usage;
        self
    }

    pub fn with_observed(mut self, observed: Vec<ObservedCooldown>) -> Self {
        self.observed = observed;
        self
    }

    pub fn with_extra_metrics(mut self, extra: String) -> Self {
        self.extra_metrics = extra;
        self
    }
}

/// A plugin's answer to an admin request.
#[derive(Debug)]
pub struct AdminReply {
    pub content_type: &'static str,
    pub body: String,
}

/// A hook declined or failed. The relay decides what to do with it.
#[derive(Debug)]
#[non_exhaustive]
pub enum PluginError {
    /// Return the status and message to the client without calling upstream. The message is public:
    /// it is sent to the client and written to Reiki's log, so never put request text or a secret in it.
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
    /// The id Reiki gives this request in its own logs, 32 hex digits; empty when the host set none.
    pub id: &'a str,
    pub stream: &'a str,
    pub upstream: &'a str,
    pub method: &'a Method,
    /// The request path as the client sent it, without the query string.
    pub path: &'a str,
    pub headers: &'a mut HeaderMap,
    pub body: &'a mut Bytes,
    pub state: &'a mut (dyn Any + Send),
    /// Findings the plugin reports for this request; the host turns them into events.
    pub reported: Vec<Reported>,
}

/// Identifiers of one kind that a detection plugin found in a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub kind: &'static str,
    /// An optional label for a name or rule within the kind. Limited to `[A-Za-z0-9_.:*/-]` and 64
    /// bytes by the host, and dropped when empty.
    pub detail: Option<String>,
    pub count: usize,
    pub blocked: bool,
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
    /// The upstream that served this response, which may differ from the request's after a fallback.
    pub upstream: &'a str,
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
            reported: Vec::new(),
        }
    }

    /// Reports `count` identifiers of one `kind` found in this request, and whether the request was
    /// blocked for them. Never pass the matched text.
    pub fn report(&mut self, kind: &'static str, count: usize, blocked: bool) {
        self.reported.push(Reported {
            kind,
            detail: None,
            count,
            blocked,
        });
    }

    /// Like [`RequestCtx::report`], with a `detail` label for one name or rule within the kind, such
    /// as a tool name. The host keeps only `[A-Za-z0-9_.:*/-]`, cuts the rest at 64 bytes and drops
    /// an empty result, so a plugin cannot send free text. Never pass the matched text.
    pub fn report_detail(&mut self, kind: &'static str, detail: &str, count: usize, blocked: bool) {
        self.reported.push(Reported {
            kind,
            detail: detail_label(detail),
            count,
            blocked,
        });
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
            upstream: "",
            status,
            headers,
            state,
        }
    }

    /// Names the upstream that served this response.
    #[must_use]
    pub fn with_upstream(mut self, upstream: &'a str) -> Self {
        self.upstream = upstream;
        self
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

/// Longest detail label kept, in bytes.
const DETAIL_MAX_BYTES: usize = 64;

/// Keeps only the documented label characters and cuts the result at a character boundary.
fn detail_label(raw: &str) -> Option<String> {
    let mut kept = String::new();
    for c in raw.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '*' | '/' | '-')) {
            continue;
        }
        if kept.len() + c.len_utf8() > DETAIL_MAX_BYTES {
            break;
        }
        kept.push(c);
    }
    (!kept.is_empty()).then_some(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_32_hex_digits_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let id = new_request_id();
            assert_eq!(id.len(), 32);
            assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
            assert!(seen.insert(id), "request id repeated");
        }
    }

    #[test]
    fn report_detail_bounds_and_filters_before_storing() {
        let method = Method::POST;
        let mut headers = HeaderMap::new();
        let mut body = Bytes::new();
        let mut state = ();
        let mut ctx = RequestCtx::new("s", "u", &method, "/x", &mut headers, &mut body, &mut state);

        let long = "a".repeat(1024 * 1024);
        ctx.report_detail("tool", &long, 1, false);
        assert_eq!(
            ctx.reported[0].detail.as_ref().map(String::len),
            Some(DETAIL_MAX_BYTES)
        );

        ctx.report_detail("tool", "bad \n name / ok", 1, false);
        assert_eq!(ctx.reported[1].detail.as_deref(), Some("badname/ok"));
        ctx.report_detail("tool", " \n ", 1, false);
        assert_eq!(ctx.reported[2].detail, None);
    }
}
