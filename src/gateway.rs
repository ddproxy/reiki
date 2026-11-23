//! Multi-user gateway foundations: per-stream limits, allowlists, fair sharing and accounting.
//!
//! Everything here is provider-agnostic. Names come from the configuration or from a request's
//! declared model; secrets, tokens and request bodies never enter a counter, a metric label or a
//! refusal message. Counters live on [`crate::proxy::App`] so a reload cannot wipe them, and the
//! durable ones are written to the same atomic state file as key health.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use reiki_plugin::{AdmissionRefusal, civil_from_days, days_from_civil};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, StoreErrorMode, StreamConfig};
use crate::proxy::App;

const SECONDS_PER_DAY: i64 = 86_400;

/// Stable reasons a gateway refusal can be counted under. Never derived from request data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RejectReason {
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

impl From<RejectReason> for reiki_plugin::GatewayRejectReason {
    fn from(reason: RejectReason) -> Self {
        match reason {
            RejectReason::Upstream => Self::Upstream,
            RejectReason::Model => Self::Model,
            RejectReason::ModelUnknown => Self::ModelUnknown,
            RejectReason::Rate => Self::Rate,
            RejectReason::Tokens => Self::Tokens,
            RejectReason::Concurrent => Self::Concurrent,
            RejectReason::QuotaRequests => Self::QuotaRequests,
            RejectReason::QuotaTokens => Self::QuotaTokens,
            RejectReason::Fairness => Self::Fairness,
            RejectReason::StoreClosed => Self::StoreClosed,
            RejectReason::CostBudget => Self::CostBudget,
        }
    }
}

impl RejectReason {
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

/// A refusal with a fixed, safe message and no request data.
#[derive(Debug, Clone, Copy)]
pub struct Refusal {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: &'static str,
    pub retry_after: Option<u64>,
    pub reason: RejectReason,
}

impl Refusal {
    fn forbidden(kind: &'static str, message: &'static str, reason: RejectReason) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            kind,
            message,
            retry_after: None,
            reason,
        }
    }

    fn busy(kind: &'static str, message: &'static str, reason: RejectReason, wait: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            kind,
            message,
            retry_after: Some(wait.max(1)),
            reason,
        }
    }

    /// Converts a plugin's static budget refusal into the same 429 shape as a limit.
    fn budget(kind: &'static str, message: &'static str) -> Self {
        Self::busy(kind, message, RejectReason::CostBudget, 60)
    }
}

/// One minute's admitted traffic. The start is Unix seconds divided by 60.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinuteCounter {
    pub start: i64,
    pub requests: u32,
    /// Tokens charged so far: the measured value when known, the request-size estimate otherwise.
    pub tokens: u64,
}

/// A daily or monthly window. The start is a Unix second in the configured zone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeriodCounter {
    pub start: i64,
    pub requests: u32,
    pub tokens: u64,
}

/// Durable counters for one stream. `token_sha256` is a hash already present in the config; it
/// lets a rotated token start with fresh counters without storing the token itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamState {
    #[serde(default)]
    pub token_sha256: String,
    #[serde(default)]
    pub requests_total: u64,
    #[serde(default)]
    pub tokens_total: u64,
    #[serde(default)]
    pub minute: MinuteCounter,
    #[serde(default)]
    pub daily: PeriodCounter,
    #[serde(default)]
    pub monthly: PeriodCounter,
}

impl StreamState {
    fn for_stream(stream: &StreamConfig) -> Self {
        Self {
            token_sha256: stream.token_sha256.to_ascii_lowercase(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    streams: BTreeMap<String, StreamState>,
    /// Concurrency is in memory only: a restart has no in-flight requests.
    active: BTreeMap<String, u32>,
    /// Last admitted request per stream, as Unix seconds. Also in memory only: it is a live
    /// freshness hint for the read-only stream console, not a durable counter.
    last_used: BTreeMap<String, u64>,
    /// Weighted round-robin counts for contested keys, reset each minute.
    fair_epoch: i64,
    fair_served: BTreeMap<(String, String), u64>,
    /// Self-service `/v1/usage` counters, separate from request admission.
    usage_minute: BTreeMap<String, MinuteCounter>,
    dirty: bool,
}

#[derive(Debug, Default)]
struct Shared {
    inner: Mutex<Inner>,
    store_ok: AtomicBool,
    store_errors: AtomicU64,
    owner: std::sync::OnceLock<Weak<App>>,
}

/// The gateway's shared state. A cheap clone is held by every admission permit.
#[derive(Debug, Default, Clone)]
pub struct Gateway {
    shared: Arc<Shared>,
}

impl Gateway {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                store_ok: AtomicBool::new(true),
                ..Shared::default()
            }),
        }
    }

    /// Lets a permit wake the state writer when its counters change.
    pub fn set_owner(&self, owner: Weak<App>) {
        let _ = self.shared.owner.set(owner);
    }

    /// Restores durable counters, discarding entries whose stream or token no longer matches.
    pub fn restore(&self, saved: BTreeMap<String, StreamState>, config: &Config) {
        let mut inner = self
            .shared
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        inner.streams = saved
            .into_iter()
            .filter(|(name, state)| {
                config.streams.get(name).is_some_and(|stream| {
                    stream
                        .token_sha256
                        .eq_ignore_ascii_case(&state.token_sha256)
                })
            })
            .collect();
        inner.dirty = false;
    }

    /// Applies a new config to live counters: streams that were removed or whose token changed
    /// lose their state, while unchanged streams keep it. Usage-route counters and fairness
    /// history follow the same rule, and the next snapshot records the change.
    pub fn reconcile(&self, config: &Config) {
        let mut inner = self.lock();
        let mut fresh = BTreeSet::new();
        for (name, stream) in &config.streams {
            let unchanged = inner.streams.get(name).is_some_and(|state| {
                stream
                    .token_sha256
                    .eq_ignore_ascii_case(&state.token_sha256)
            });
            if !unchanged {
                // Keep a zeroed row so a rotated token still sees its own usage, not an
                // empty list or the previous tenant's counters.
                inner
                    .streams
                    .insert(name.clone(), StreamState::for_stream(stream));
                fresh.insert(name.clone());
            }
        }
        inner
            .streams
            .retain(|name, _| config.streams.contains_key(name));
        inner
            .last_used
            .retain(|name, _| config.streams.contains_key(name));
        inner
            .usage_minute
            .retain(|name, _| config.streams.contains_key(name) && !fresh.contains(name));
        inner
            .fair_served
            .retain(|(name, _), _| config.streams.contains_key(name) && !fresh.contains(name));
        inner.dirty = true;
    }

    /// The durable snapshot written by `App::persist_state_now`.
    pub fn snapshot(&self) -> BTreeMap<String, StreamState> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .streams
            .clone()
    }

    /// True while counters can be read and written reliably.
    pub fn store_ok(&self) -> bool {
        self.shared.store_ok.load(Ordering::Acquire)
    }

    pub fn store_errors(&self) -> u64 {
        self.shared.store_errors.load(Ordering::Relaxed)
    }

    /// Marks counter state unavailable, for example when it could not be read at startup.
    pub fn store_failed(&self) {
        self.shared.store_ok.store(false, Ordering::Release);
    }

    /// Marks counter state available again after a successful write.
    fn store_succeeded(&self) {
        self.shared.store_ok.store(true, Ordering::Release);
    }

    pub fn store_recovered(&self) {
        self.store_succeeded();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn take_dirty(&self) -> bool {
        std::mem::take(&mut self.lock().dirty)
    }

    /// Counts a closed-store refusal. Open mode counts the event without refusing it.
    pub fn note_store_error(&self, stream: &str, closed: bool) -> Option<Refusal> {
        self.shared.store_errors.fetch_add(1, Ordering::Relaxed);
        if closed {
            Some(Refusal {
                status: StatusCode::SERVICE_UNAVAILABLE,
                kind: "gateway_store_unavailable",
                message: "per-stream counters are unavailable",
                retry_after: None,
                reason: RejectReason::StoreClosed,
            })
        } else {
            let _ = stream;
            None
        }
    }

    /// Checks the self-service usage route's own rate limit. Returns a wait in seconds when over.
    pub fn usage_rpm_wait(&self, stream: &str, config: &Config, now: SystemTime) -> Option<u64> {
        let limit = config.gateway.usage_rpm;
        let second = unix_seconds(now);
        let minute = second.div_euclid(60);
        let mut inner = self.lock();
        let entry = inner.usage_minute.entry(stream.to_owned()).or_default();
        if entry.start != minute {
            *entry = MinuteCounter {
                start: minute,
                ..MinuteCounter::default()
            };
        }
        if u64::from(entry.requests) >= u64::from(limit) {
            return Some((((minute + 1) * 60 - second).max(1)) as u64);
        }
        entry.requests += 1;
        None
    }

    /// Validates allowlists and limits, then reserves one request and one concurrency slot.
    #[allow(clippy::too_many_arguments)]
    pub fn admit(
        &self,
        config: &Config,
        stream: &str,
        upstream: &str,
        key: &str,
        model: Option<&str>,
        now: SystemTime,
        estimate: u64,
    ) -> Result<Permit, Refusal> {
        self.admit_with_budget(config, stream, upstream, key, model, now, estimate, None)
    }

    /// Like [`Self::admit`], with a plugin budget refusal checked before any counters move.
    #[allow(clippy::too_many_arguments)]
    pub fn admit_with_budget(
        &self,
        config: &Config,
        stream: &str,
        upstream: &str,
        key: &str,
        model: Option<&str>,
        now: SystemTime,
        estimate: u64,
        budget: Option<AdmissionRefusal>,
    ) -> Result<Permit, Refusal> {
        let Some(settings) = config.streams.get(stream) else {
            return Err(Refusal::forbidden(
                "gateway_forbidden",
                "this stream is not configured",
                RejectReason::Upstream,
            ));
        };
        let closed = settings
            .limits
            .as_ref()
            .and_then(|limits| limits.on_store_error)
            .unwrap_or(config.gateway.on_store_error);
        if !self.store_ok() {
            let refusal = self.note_store_error(stream, closed == StoreErrorMode::Closed);
            if let Some(refusal) = refusal {
                return Err(refusal);
            }
        }

        if let Some(patterns) = &settings.allowed_upstreams
            && !patterns
                .iter()
                .any(|pattern| glob_matches(pattern, upstream))
        {
            return Err(Refusal::forbidden(
                "gateway_upstream_forbidden",
                "this upstream is not allowed for the stream",
                RejectReason::Upstream,
            ));
        }
        if let Some(patterns) = &settings.allowed_models {
            let Some(model) = model else {
                return Err(Refusal::forbidden(
                    "gateway_model_unknown",
                    "the request model could not be determined",
                    RejectReason::ModelUnknown,
                ));
            };
            if !patterns.iter().any(|pattern| glob_matches(pattern, model)) {
                return Err(Refusal::forbidden(
                    "gateway_model_forbidden",
                    "this model is not allowed for the stream",
                    RejectReason::Model,
                ));
            }
        }

        // A cost budget is checked here, before any counter is reserved, so a refusal
        // is indistinguishable from a configured stream limit to the rest of the relay.
        if let Some(refusal) = budget {
            return Err(Refusal::budget(refusal.kind, refusal.message));
        }

        let second = unix_seconds(now);
        let minute = second.div_euclid(60);
        let retry_after = (((minute + 1) * 60 - second).max(1)) as u64;
        let zone = settings
            .limits
            .as_ref()
            .and_then(|limits| limits.period_zone.as_deref())
            .unwrap_or("UTC");
        let day_start = period_start(second, zone, false);
        let month_start = period_start(second, zone, true);

        let mut inner = self.lock();
        {
            // Bring the state up to date in a short block so the checks below can borrow
            // `inner` freely.
            let state = inner
                .streams
                .entry(stream.to_owned())
                .or_insert_with(|| StreamState::for_stream(settings));
            if state.token_sha256.is_empty() {
                state.token_sha256 = settings.token_sha256.to_ascii_lowercase();
            }
            if state.minute.start != minute {
                state.minute = MinuteCounter {
                    start: minute,
                    ..MinuteCounter::default()
                };
            }
            if state.daily.start != day_start {
                state.daily = PeriodCounter {
                    start: day_start,
                    ..PeriodCounter::default()
                };
            }
            if state.monthly.start != month_start {
                state.monthly = PeriodCounter {
                    start: month_start,
                    ..PeriodCounter::default()
                };
            }
        }

        if let Some(limits) = &settings.limits {
            let state = &inner.streams[stream];
            if let Some(rpm) = limits.rpm
                && state.minute.requests >= rpm
            {
                return Err(Refusal::busy(
                    "gateway_rate_limited",
                    "the stream's request rate limit was reached",
                    RejectReason::Rate,
                    retry_after,
                ));
            }
            if let Some(tpm) = limits.tpm
                && state.minute.tokens.saturating_add(estimate) > tpm
            {
                return Err(Refusal::busy(
                    "gateway_token_limited",
                    "the stream's token rate limit was reached",
                    RejectReason::Tokens,
                    retry_after,
                ));
            }
            if let Some(limit) = limits.daily_requests
                && state.daily.requests >= limit
            {
                return Err(Refusal::busy(
                    "gateway_daily_requests",
                    "the stream's daily request limit was reached",
                    RejectReason::QuotaRequests,
                    retry_after,
                ));
            }
            if let Some(limit) = limits.daily_tokens
                && state.daily.tokens.saturating_add(estimate) > limit
            {
                return Err(Refusal::busy(
                    "gateway_daily_tokens",
                    "the stream's daily token limit was reached",
                    RejectReason::QuotaTokens,
                    retry_after,
                ));
            }
            if let Some(limit) = limits.monthly_requests
                && state.monthly.requests >= limit
            {
                return Err(Refusal::busy(
                    "gateway_monthly_requests",
                    "the stream's monthly request limit was reached",
                    RejectReason::QuotaRequests,
                    retry_after,
                ));
            }
            if let Some(limit) = limits.monthly_tokens
                && state.monthly.tokens.saturating_add(estimate) > limit
            {
                return Err(Refusal::busy(
                    "gateway_monthly_tokens",
                    "the stream's monthly token limit was reached",
                    RejectReason::QuotaTokens,
                    retry_after,
                ));
            }
            if let Some(limit) = limits.concurrent
                && inner.active.get(stream).copied().unwrap_or(0) >= limit
            {
                return Err(Refusal::busy(
                    "gateway_concurrent_limited",
                    "the stream has too many requests in flight",
                    RejectReason::Concurrent,
                    1,
                ));
            }
        }

        if inner.fair_epoch != minute {
            inner.fair_epoch = minute;
            inner.fair_served.clear();
        }
        if fairness_refusal(config, stream, key, upstream, model, &inner).is_some() {
            return Err(Refusal::busy(
                "gateway_fairness_limited",
                "the shared key is serving another stream's turn",
                RejectReason::Fairness,
                retry_after,
            ));
        }

        inner
            .fair_served
            .entry((key.to_owned(), stream.to_owned()))
            .and_modify(|served| *served += 1)
            .or_insert(1);
        {
            let state = inner
                .streams
                .get_mut(stream)
                .expect("the entry was inserted above");
            state.requests_total += 1;
            state.minute.requests += 1;
            state.daily.requests += 1;
            state.monthly.requests += 1;
            // Charge the estimate so an in-flight request cannot bypass a token budget. It is
            // replaced by the measured value when the body ends.
            state.minute.tokens = state.minute.tokens.saturating_add(estimate);
            state.daily.tokens = state.daily.tokens.saturating_add(estimate);
            state.monthly.tokens = state.monthly.tokens.saturating_add(estimate);
            state.tokens_total = state.tokens_total.saturating_add(estimate);
        }
        *inner.active.entry(stream.to_owned()).or_default() += 1;
        inner
            .last_used
            .insert(stream.to_owned(), second.max(0) as u64);
        inner.dirty = true;
        drop(inner);

        Ok(Permit {
            gateway: self.clone(),
            owner: self.shared.owner.get().cloned(),
            stream: stream.to_owned(),
            estimate,
            done: false,
        })
    }

    /// A copy of one stream's durable counters for usage views.
    pub fn stream_state(&self, stream: &str) -> StreamState {
        self.lock().streams.get(stream).cloned().unwrap_or_default()
    }

    /// All durable counters, for the admin view.
    pub fn all_states(&self) -> BTreeMap<String, StreamState> {
        self.lock().streams.clone()
    }

    /// Requests currently in flight for one stream.
    pub fn active_count(&self, stream: &str) -> u32 {
        self.lock().active.get(stream).copied().unwrap_or_default()
    }

    /// Unix seconds of the last admitted request, when the stream has served one this process.
    pub fn last_used(&self, stream: &str) -> Option<u64> {
        self.lock().last_used.get(stream).copied()
    }

    /// True when a write is pending, so the caller can wake the state writer.
    pub fn flush_if_dirty(&self) {
        if self.take_dirty()
            && let Some(owner) = self.shared.owner.get().and_then(Weak::upgrade)
        {
            owner.persist_state();
        }
    }

    pub fn write_succeeded(&self) {
        self.store_succeeded();
    }

    pub fn write_failed(&self) {
        self.store_failed();
    }
}

/// Reserved once a request is admitted; holds the concurrency slot until its body ends.
#[derive(Debug)]
pub struct Permit {
    gateway: Gateway,
    owner: Option<Weak<App>>,
    stream: String,
    estimate: u64,
    done: bool,
}

impl Permit {
    /// Replaces the estimate with the final usage and releases the slot. Safe to call once.
    pub fn complete(&mut self, tokens: u64) {
        if self.done {
            return;
        }
        self.done = true;
        let second = unix_seconds(SystemTime::now());
        let minute = second.div_euclid(60);
        let settings = self
            .owner
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|app| app.snapshot());
        let zone = settings
            .as_ref()
            .and_then(|snapshot| snapshot.config.streams.get(&self.stream))
            .and_then(|stream| stream.limits.as_ref())
            .and_then(|limits| limits.period_zone.as_deref())
            .unwrap_or("UTC");
        let day_start = period_start(second, zone, false);
        let month_start = period_start(second, zone, true);
        let (minute_start, day_window_start, month_window_start) = {
            let inner = self.gateway.lock();
            inner
                .streams
                .get(&self.stream)
                .map_or((minute, day_start, month_start), |state| {
                    (state.minute.start, state.daily.start, state.monthly.start)
                })
        };
        {
            let mut inner = self.gateway.lock();
            if let Some(state) = inner.streams.get_mut(&self.stream) {
                // Only adjust the current windows; a long body that crosses a boundary keeps
                // its charge in the window where it was admitted.
                adjust(
                    &mut state.minute,
                    minute_start,
                    minute,
                    self.estimate,
                    tokens,
                );
                adjust(
                    &mut state.daily,
                    day_window_start,
                    day_start,
                    self.estimate,
                    tokens,
                );
                adjust(
                    &mut state.monthly,
                    month_window_start,
                    month_start,
                    self.estimate,
                    tokens,
                );
                state.tokens_total = state.tokens_total.saturating_sub(self.estimate) + tokens;
            }
            if let Some(active) = inner.active.get_mut(&self.stream) {
                *active = active.saturating_sub(1);
                if *active == 0 {
                    inner.active.remove(&self.stream);
                }
            }
            inner.dirty = true;
        }
        self.wake_writer();
    }

    fn wake_writer(&self) {
        if let Some(owner) = self.owner.as_ref().and_then(Weak::upgrade) {
            owner.persist_state();
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        {
            let mut inner = self.gateway.lock();
            if let Some(active) = inner.active.get_mut(&self.stream) {
                *active = active.saturating_sub(1);
                if *active == 0 {
                    inner.active.remove(&self.stream);
                }
            }
            inner.dirty = true;
        }
        self.wake_writer();
    }
}

fn adjust<T>(counter: &mut T, stored_start: i64, expected_start: i64, estimate: u64, actual: u64)
where
    T: HasTokens,
{
    if stored_start == expected_start {
        counter.set_tokens(
            counter
                .tokens()
                .saturating_sub(estimate)
                .saturating_add(actual),
        );
    }
}

trait HasTokens {
    fn tokens(&self) -> u64;
    fn set_tokens(&mut self, tokens: u64);
}

impl HasTokens for MinuteCounter {
    fn tokens(&self) -> u64 {
        self.tokens
    }
    fn set_tokens(&mut self, tokens: u64) {
        self.tokens = tokens;
    }
}

impl HasTokens for PeriodCounter {
    fn tokens(&self) -> u64 {
        self.tokens
    }
    fn set_tokens(&mut self, tokens: u64) {
        self.tokens = tokens;
    }
}

/// Weighted round-robin among streams with recent traffic on this key. A stream that has not
/// used the key yet is not a contender, so a single tenant is never throttled for no reason.
fn fairness_refusal(
    config: &Config,
    stream: &str,
    key: &str,
    upstream: &str,
    model: Option<&str>,
    inner: &Inner,
) -> Option<()> {
    let mut ratios: BTreeMap<&str, (u64, u32)> = BTreeMap::new();
    for (name, settings) in &config.streams {
        if !settings.chain.iter().any(|candidate| candidate == key) {
            continue;
        }
        if let Some(patterns) = &settings.allowed_upstreams
            && !patterns
                .iter()
                .any(|pattern| glob_matches(pattern, upstream))
        {
            continue;
        }
        if let Some(patterns) = &settings.allowed_models {
            let Some(model) = model else {
                continue;
            };
            if !patterns.iter().any(|pattern| glob_matches(pattern, model)) {
                continue;
            }
        }
        let served = inner
            .fair_served
            .get(&(key.to_owned(), name.clone()))
            .copied()
            .unwrap_or(0);
        if served > 0 || name == stream {
            ratios.insert(name.as_str(), (served, settings.weight.max(1)));
        }
    }
    if ratios.len() < 2 {
        return None;
    }
    let mine = *ratios.get(stream)?;
    let min = ratios
        .into_values()
        .map(|(served, weight)| served / u64::from(weight))
        .min()?;
    (mine.0 / u64::from(mine.1) > min).then_some(())
}

/// A small, allocation-free glob matcher supporting `*` and `?`. Patterns are validated at
/// config load, so every string here is safe to use.
/// `*` matches any run (including empty), `?` matches exactly one character.
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for &byte in pattern {
        let mut current = vec![false; value.len() + 1];
        match byte {
            b'*' => {
                current[0] = previous[0];
                for index in 1..=value.len() {
                    current[index] = previous[index] || current[index - 1];
                }
            }
            b'?' => {
                current[1..].copy_from_slice(&previous[..value.len()]);
            }
            other => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && value[index - 1] == other;
                }
            }
        }
        previous = current;
    }
    previous[value.len()]
}

fn unix_seconds(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Start of the current day or month, in the configured fixed offset.
fn period_start(now_seconds: i64, zone: &str, monthly: bool) -> i64 {
    let offset = crate::quota::parse_zone(zone).unwrap_or(0);
    let local = now_seconds + offset;
    let day = local.div_euclid(SECONDS_PER_DAY);
    let start_day = if monthly {
        let (year, month, _) = civil_from_days(day);
        days_from_civil(year, month, 1)
    } else {
        day
    };
    start_day * SECONDS_PER_DAY - offset
}

/// A JSON view for self-service or admin usage. Names are configured stream names only.
/// `cost` is inserted only into the row of `only`, which the caller has authenticated.
pub fn usage_view(
    states: &BTreeMap<String, StreamState>,
    only: Option<&str>,
    cost: Option<&Value>,
) -> String {
    let mut out = String::from("{\"streams\":[");
    let mut first = true;
    for (name, state) in states {
        if only.is_some_and(|wanted| wanted != name) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str("{\"name\":");
        out.push_str(&json_string(name));
        out.push_str(",\"requests\":");
        out.push_str(&state.requests_total.to_string());
        out.push_str(",\"tokens\":");
        out.push_str(&state.tokens_total.to_string());
        out.push_str(",\"minute_requests\":");
        out.push_str(&state.minute.requests.to_string());
        out.push_str(",\"minute_tokens\":");
        out.push_str(&state.minute.tokens.to_string());
        if let (Some(wanted), Some(fragment)) = (only, cost)
            && wanted == name
        {
            out.push_str(",\"cost\":");
            out.push_str(&fragment.to_string());
        }
        out.push('}');
    }
    out.push_str("]}");
    out
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Seconds until a stream's next daily or monthly window, for status views.
pub fn window_remaining(now: SystemTime, zone: &str, monthly: bool) -> u64 {
    let offset = crate::quota::parse_zone(zone).unwrap_or(0);
    let local = unix_seconds(now) + offset;
    let next = if monthly {
        let day = local.div_euclid(SECONDS_PER_DAY);
        let (year, month, _) = civil_from_days(day);
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        days_from_civil(next_year, next_month, 1) * SECONDS_PER_DAY - offset
    } else {
        period_start(unix_seconds(now), zone, false) + SECONDS_PER_DAY
    };
    (next - unix_seconds(now)).max(0) as u64
}

/// Active request count for a stream, published as a gauge without a stream name in metrics.
pub fn active_count(gateway: &Gateway, stream: &str) -> u32 {
    let inner = gateway.lock();
    inner.active.get(stream).copied().unwrap_or(0)
}

/// Fixed-name summary used by peer health: counts only, never names or model strings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GatewayCounts {
    pub requests: u64,
    pub tokens: u64,
    pub store_errors: u64,
}

impl GatewayCounts {
    pub fn sample(gateway: &Gateway) -> Self {
        Self {
            requests: gateway
                .all_states()
                .values()
                .map(|state| state.requests_total)
                .sum(),
            tokens: gateway
                .all_states()
                .values()
                .map(|state| state.tokens_total)
                .sum(),
            store_errors: gateway.store_errors(),
        }
    }
}

/// A set of all streams that may use a key, used by tests and fairness diagnostics.
pub fn eligible_streams(config: &Config, key: &str) -> BTreeSet<String> {
    config
        .streams
        .iter()
        .filter(|(_, stream)| stream.chain.iter().any(|candidate| candidate == key))
        .map(|(name, _)| name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn config(extra: &str) -> Config {
        Config::parse(&format!(
            "[upstream.zen]\nbase_url = \"https://example.com\"\n\
             [keys.k1]\nupstream = \"zen\"\nfile = \"/k1\"\n\
             [keys.k2]\nupstream = \"zen\"\nfile = \"/k2\"\n{extra}"
        ))
        .unwrap()
    }

    fn stream(extra: &str) -> Config {
        stream_with_hash(HASH_A, extra)
    }

    fn stream_with_hash(hash: &str, extra: &str) -> Config {
        config(&format!(
            "[streams.s]\ntoken_sha256 = \"{hash}\"\nchain = [\"k1\", \"k2\"]\n{extra}"
        ))
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + std::time::Duration::from_secs(1_704_155_400)
    }

    fn admit(gateway: &Gateway, config: &Config, stream: &str) -> Permit {
        gateway
            .admit(config, stream, "zen", "k1", Some("m"), now(), 10)
            .expect("admission succeeds")
    }

    #[test]
    fn globs_match_names_without_allocating() {
        assert!(glob_matches("zen-*", "zen-a"));
        assert!(!glob_matches("zen-*", "other-a"));
        assert!(glob_matches("gpt-?-turbo", "gpt-4-turbo"));
        assert!(!glob_matches("gpt-?-turbo", "gpt-44-turbo"));
        assert!(!crate::config::valid_glob(""));
    }

    #[test]
    fn periods_start_at_local_midnight() {
        // 2024-01-02 00:30 UTC is 2024-01-01 22:30 at -02:00.
        let seconds = 1_704_155_400;
        let day = period_start(seconds, "-02:00", false);
        assert_eq!(day, 1_704_067_200 + 7_200);
        let month = period_start(seconds, "UTC", true);
        assert_eq!(month, 1_704_067_200);
    }

    #[test]
    fn an_rpm_limit_refuses_the_next_request_in_the_minute() {
        let config = stream("[streams.s.limits]\nrpm = 2\n");
        let gateway = Gateway::new();
        let _first = admit(&gateway, &config, "s");
        let _second = admit(&gateway, &config, "s");
        let refusal = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
            .expect_err("the third request is refused");
        assert_eq!(refusal.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refusal.reason, RejectReason::Rate);
        assert_eq!(refusal.kind, "gateway_rate_limited");
        assert!(refusal.retry_after.is_some_and(|wait| wait >= 1));
        assert_eq!(gateway.stream_state("s").requests_total, 2);
    }

    #[test]
    fn a_tpm_limit_counts_the_estimate_and_refuses_when_exceeded() {
        let config = stream("[streams.s.limits]\ntpm = 25\n");
        let gateway = Gateway::new();
        let first = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
            .unwrap();
        let refusal = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 16)
            .expect_err("10 + 16 exceeds 25");
        assert_eq!(refusal.reason, RejectReason::Tokens);
        drop(first);
        assert_eq!(gateway.stream_state("s").minute.tokens, 10);
    }

    #[test]
    fn concurrent_slots_are_held_until_the_permit_completes() {
        let config = stream("[streams.s.limits]\nconcurrent = 1\n");
        let gateway = Gateway::new();
        let mut first = admit(&gateway, &config, "s");
        let refusal = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
            .expect_err("the slot is taken");
        assert_eq!(refusal.reason, RejectReason::Concurrent);
        assert_eq!(refusal.kind, "gateway_concurrent_limited");
        assert_eq!(active_count(&gateway, "s"), 1);
        first.complete(10);
        assert!(
            gateway
                .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
                .is_ok()
        );
    }

    #[test]
    fn a_permit_completes_only_once_and_releases_one_slot() {
        let config = stream("[streams.s.limits]\nconcurrent = 1\n");
        let gateway = Gateway::new();
        let mut permit = admit(&gateway, &config, "s");
        permit.complete(10);
        permit.complete(10);
        assert_eq!(
            gateway.stream_state("s").tokens_total,
            10,
            "no double charge"
        );
        assert_eq!(active_count(&gateway, "s"), 0);
        drop(permit);
        assert_eq!(active_count(&gateway, "s"), 0);
    }

    #[test]
    fn final_usage_replaces_the_estimate_in_every_window() {
        let config = stream("");
        let gateway = Gateway::new();
        let current = SystemTime::now();
        let mut permit = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), current, 10)
            .unwrap();
        permit.complete(40);
        let state = gateway.stream_state("s");
        assert_eq!(state.minute.tokens, 40);
        assert_eq!(state.daily.tokens, 40);
        assert_eq!(state.monthly.tokens, 40);
        assert_eq!(state.tokens_total, 40);
    }

    #[test]
    fn daily_and_monthly_quotas_count_requests_and_tokens() {
        // Token quotas refuse before the next request would exceed a request quota.
        let config = stream(
            "[streams.s.limits]\ndaily_requests = 10\ndaily_tokens = 25\n\
             monthly_requests = 20\nmonthly_tokens = 45\n",
        );
        let gateway = Gateway::new();
        assert!(
            gateway
                .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
                .is_ok()
        );
        assert!(
            gateway
                .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
                .is_ok()
        );
        let tokens = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
            .expect_err("10 more would exceed the daily token quota");
        assert_eq!(tokens.reason, RejectReason::QuotaTokens);
        let state = gateway.stream_state("s");
        assert_eq!(state.daily.requests, 2);
        assert_eq!(state.monthly.requests, 2);
        assert_eq!(state.daily.tokens, 20);

        // Separate request quotas refuse once their count is reached.
        let config = stream("[streams.s.limits]\ndaily_requests = 2\nmonthly_requests = 5\n");
        let gateway = Gateway::new();
        let _ = admit(&gateway, &config, "s");
        let _ = admit(&gateway, &config, "s");
        let refusal = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
            .expect_err("the daily request quota is reached");
        assert_eq!(refusal.reason, RejectReason::QuotaRequests);
        assert_eq!(refusal.kind, "gateway_daily_requests");
    }

    #[test]
    fn a_stream_not_in_the_config_is_forbidden() {
        let config = stream("");
        let gateway = Gateway::new();
        let refusal = gateway
            .admit(&config, "nobody", "zen", "k1", Some("m"), now(), 10)
            .expect_err("unknown streams are refused");
        assert_eq!(refusal.status, StatusCode::FORBIDDEN);
        assert_eq!(refusal.reason, RejectReason::Upstream);
    }

    #[test]
    fn allowed_upstreams_deny_a_mismatch_and_an_empty_list_denies_all() {
        let allowed = stream("allowed_upstreams = [\"other-*\"]\n");
        let gateway = Gateway::new();
        let refusal = gateway
            .admit(&allowed, "s", "zen", "k1", Some("m"), now(), 10)
            .expect_err("zen is not allowed");
        assert_eq!(refusal.status, StatusCode::FORBIDDEN);
        assert_eq!(refusal.kind, "gateway_upstream_forbidden");
        assert_eq!(refusal.reason, RejectReason::Upstream);

        let empty = stream("allowed_upstreams = []\n");
        assert!(
            Gateway::new()
                .admit(&empty, "s", "zen", "k1", Some("m"), now(), 10)
                .is_err()
        );

        let glob = stream("allowed_upstreams = [\"ze?\"]\n");
        assert!(
            Gateway::new()
                .admit(&glob, "s", "zen", "k1", Some("m"), now(), 10)
                .is_ok()
        );
    }

    #[test]
    fn allowed_models_deny_an_unknown_or_forbidden_model() {
        let allowed = stream("allowed_models = [\"gpt-*\"]\n");
        let gateway = Gateway::new();
        let unknown = gateway
            .admit(&allowed, "s", "zen", "k1", None, now(), 10)
            .expect_err("no model is refused when the list is set");
        assert_eq!(unknown.kind, "gateway_model_unknown");
        assert_eq!(unknown.reason, RejectReason::ModelUnknown);
        let forbidden = gateway
            .admit(&allowed, "s", "zen", "k1", Some("claude"), now(), 10)
            .expect_err("a model outside the list is refused");
        assert_eq!(forbidden.kind, "gateway_model_forbidden");
        assert_eq!(forbidden.reason, RejectReason::Model);

        let empty = stream("allowed_models = []\n");
        assert!(
            Gateway::new()
                .admit(&empty, "s", "zen", "k1", Some("gpt-x"), now(), 10)
                .is_err()
        );
        // With no allowlist, a missing model is fine.
        assert!(
            Gateway::new()
                .admit(&stream(""), "s", "zen", "k1", None, now(), 10)
                .is_ok()
        );
    }

    #[test]
    fn fairness_only_engages_among_streams_with_recent_traffic() {
        let config = config(&format!(
            "[streams.a]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"k1\"]\n\
             [streams.b]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"k1\"]\n"
        ));
        let gateway = Gateway::new();
        // One stream alone is never throttled for fairness.
        for _ in 0..5 {
            assert!(
                gateway
                    .admit(&config, "a", "zen", "k1", Some("m"), now(), 1)
                    .is_ok()
            );
        }
        // The second stream is a newcomer, so it gets a turn before a is refused.
        assert!(
            gateway
                .admit(&config, "b", "zen", "k1", Some("m"), now(), 1)
                .is_ok()
        );
        let refusal = gateway
            .admit(&config, "a", "zen", "k1", Some("m"), now(), 1)
            .expect_err("a has served more than its fair share");
        assert_eq!(refusal.reason, RejectReason::Fairness);
        assert_eq!(refusal.kind, "gateway_fairness_limited");
    }

    #[test]
    fn fairness_respects_configured_weights() {
        let config = config(&format!(
            "[streams.a]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"k1\"]\nweight = 3\n\
             [streams.b]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"k1\"]\n"
        ));
        let gateway = Gateway::new();
        // a may serve three times before b catches up.
        for _ in 0..3 {
            assert!(
                gateway
                    .admit(&config, "a", "zen", "k1", Some("m"), now(), 1)
                    .is_ok()
            );
        }
        assert!(
            gateway
                .admit(&config, "b", "zen", "k1", Some("m"), now(), 1)
                .is_ok()
        );
        assert!(
            gateway
                .admit(&config, "b", "zen", "k1", Some("m"), now(), 1)
                .is_ok()
        );
    }

    #[test]
    fn a_closed_store_refuses_every_admission() {
        let config = stream("");
        let gateway = Gateway::new();
        gateway.store_failed();
        let refusal = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
            .expect_err("closed mode refuses");
        assert_eq!(refusal.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refusal.kind, "gateway_store_unavailable");
        assert_eq!(refusal.reason, RejectReason::StoreClosed);
        assert_eq!(gateway.store_errors(), 1);
    }

    #[test]
    fn an_open_store_serves_and_counts_the_error() {
        let config = Config::parse(&format!(
            "[gateway]\non_store_error = \"open\"\n\
             [upstream.zen]\nbase_url = \"https://example.com\"\n\
             [keys.k1]\nupstream = \"zen\"\nfile = \"/k1\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"k1\"]\n"
        ))
        .unwrap();
        let gateway = Gateway::new();
        gateway.store_failed();
        assert!(
            gateway
                .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
                .is_ok()
        );
        assert_eq!(gateway.store_errors(), 1);
        assert_eq!(gateway.stream_state("s").requests_total, 1);
    }

    #[test]
    fn live_usage_tracks_last_use_and_in_flight_requests() {
        let config = stream("");
        let gateway = Gateway::new();
        assert_eq!(gateway.last_used("s"), None);
        assert_eq!(gateway.active_count("s"), 0);
        let mut permit = gateway
            .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
            .expect("admission succeeds");
        assert!(gateway.last_used("s").is_some_and(|second| second > 0));
        assert_eq!(gateway.active_count("s"), 1);
        permit.complete(10);
        assert_eq!(gateway.active_count("s"), 0);
        assert!(gateway.last_used("s").is_some());
    }

    #[test]
    fn a_per_stream_store_mode_overrides_the_default() {
        let config = stream("[streams.s.limits]\non_store_error = \"open\"\n");
        let gateway = Gateway::new();
        gateway.store_failed();
        assert!(
            gateway
                .admit(&config, "s", "zen", "k1", Some("m"), now(), 10)
                .is_ok()
        );
        assert_eq!(gateway.store_errors(), 1);
    }

    #[test]
    fn the_usage_route_has_its_own_rate_limit() {
        let config = Config::parse(&format!(
            "[gateway]\nusage_rpm = 2\n\
             [upstream.zen]\nbase_url = \"https://example.com\"\n\
             [keys.k1]\nupstream = \"zen\"\nfile = \"/k1\"\n\
             [streams.s]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"k1\"]\n"
        ))
        .unwrap();
        let gateway = Gateway::new();
        assert!(gateway.usage_rpm_wait("s", &config, now()).is_none());
        assert!(gateway.usage_rpm_wait("s", &config, now()).is_none());
        assert!(gateway.usage_rpm_wait("s", &config, now()).is_some());
        // Other streams have their own budget.
        assert!(gateway.usage_rpm_wait("other", &config, now()).is_none());
        // The next minute resets the budget.
        assert!(
            gateway
                .usage_rpm_wait("s", &config, now() + std::time::Duration::from_secs(60))
                .is_none()
        );
    }

    #[test]
    fn durable_state_restores_only_matching_streams_and_tokens() {
        let gateway = Gateway::new();
        let config = stream("");
        admit(&gateway, &config, "s");
        let mut saved = gateway.snapshot();
        saved.insert(
            "gone".to_owned(),
            StreamState {
                token_sha256: HASH_B.to_owned(),
                requests_total: 9,
                ..StreamState::default()
            },
        );
        let rotated = stream_with_hash(HASH_C, "");
        gateway.restore(saved, &rotated);
        let state = gateway.stream_state("s");
        assert_eq!(state.requests_total, 0, "a rotated token starts fresh");
        assert_eq!(gateway.stream_state("gone").requests_total, 0);
    }

    #[test]
    fn durable_state_keeps_counters_for_a_matching_token() {
        let gateway = Gateway::new();
        let config = stream("");
        admit(&gateway, &config, "s");
        gateway.restore(gateway.snapshot(), &config);
        assert_eq!(gateway.stream_state("s").requests_total, 1);
    }

    #[test]
    fn reload_reconciles_rotated_tokens_and_removed_streams() {
        let gateway = Gateway::new();
        let config = stream(&format!(
            "[streams.gone]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"k1\"]\n"
        ));
        admit(&gateway, &config, "s");
        admit(&gateway, &config, "gone");
        gateway.usage_rpm_wait("s", &config, now());
        gateway.usage_rpm_wait("gone", &config, now());

        let same = stream(&format!(
            "[streams.gone]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"k1\"]\n"
        ));
        gateway.reconcile(&same);
        assert_eq!(gateway.stream_state("s").requests_total, 1);
        assert_eq!(gateway.stream_state("gone").requests_total, 1);

        // A rotated token loses its counters, its usage budget and its fairness history.
        gateway.reconcile(&stream_with_hash(HASH_C, ""));
        let rotated_state = gateway.stream_state("s");
        assert_eq!(rotated_state.requests_total, 0);
        assert_eq!(rotated_state.token_sha256, HASH_C.to_ascii_lowercase());
        assert!(
            gateway.stream_state("gone").token_sha256.is_empty(),
            "a removed stream has no state at all"
        );
        let inner = gateway.shared.inner.lock().unwrap();
        assert!(inner.usage_minute.is_empty());
        assert!(inner.fair_served.is_empty());
        assert!(inner.dirty);
    }

    #[test]
    fn a_poisoned_lock_still_admits_and_settles() {
        let gateway = Gateway::new();
        let config = stream("");
        let poison = gateway.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison.shared.inner.lock().unwrap();
            panic!("poison the gateway lock");
        })
        .join();
        assert!(gateway.shared.inner.is_poisoned());
        // Every lock goes through `lock()`, which recovers a poisoned guard.
        let _permit = admit(&gateway, &config, "s");
        assert_eq!(gateway.stream_state("s").requests_total, 1);
    }

    #[test]
    fn usage_view_shows_only_the_requested_stream() {
        let gateway = Gateway::new();
        let config = config(&format!(
            "[streams.a]\ntoken_sha256 = \"{HASH_A}\"\nchain = [\"k1\"]\n\
             [streams.b]\ntoken_sha256 = \"{HASH_B}\"\nchain = [\"k1\"]\n"
        ));
        admit(&gateway, &config, "a");
        admit(&gateway, &config, "b");
        let all = usage_view(&gateway.all_states(), None, None);
        assert!(all.contains("\"name\":\"a\""));
        assert!(all.contains("\"name\":\"b\""));
        let one = usage_view(&gateway.all_states(), Some("a"), None);
        assert!(one.contains("\"name\":\"a\""));
        assert!(!one.contains("\"name\":\"b\""));
    }

    #[test]
    fn window_remaining_is_always_positive() {
        for zone in ["UTC", "+02:00", "-05:30"] {
            assert!(window_remaining(now(), zone, false) >= 1);
            assert!(window_remaining(now(), zone, true) >= 1);
        }
    }
}
