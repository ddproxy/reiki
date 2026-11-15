use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::classify::FailureClass;
use reiki_plugin::CooldownScope;

/// A route and model combination a failure can be confined to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scope {
    pub route: String,
    pub model: String,
}

/// Collapse id-like path segments so `/files/123` and `/files/456` share a scope.
pub fn scope_route(rest: &str) -> String {
    let path = rest.split('?').next().unwrap_or(rest);
    let mut out = String::with_capacity(path.len());
    for (index, segment) in path.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        let id_like = !segment.is_empty()
            && (segment.bytes().all(|b| b.is_ascii_digit()) || segment.chars().count() > 24);
        if id_like {
            out.push('*');
        } else {
            out.push_str(segment);
        }
    }
    // The route comes from the request line, so bound what a stored combination can hold.
    let mut end = out.len().min(MAX_ROUTE_BYTES);
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out
}

/// A cooling combination, soonest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoolingScope {
    pub scope: Scope,
    pub remaining: Duration,
    pub class: FailureClass,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub rate_limited_default: Duration,
    pub quota_exhausted: Duration,
    pub transient: Duration,
    pub transient_threshold: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            rate_limited_default: Duration::from_secs(60),
            quota_exhausted: Duration::from_secs(5 * 3600),
            transient: Duration::from_secs(30),
            transient_threshold: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Sticky,
    RoundRobin,
}

#[derive(Debug, Clone, Copy)]
struct ScopeHealth {
    cooling_until: Instant,
    class: FailureClass,
}

/// Most combinations one key remembers. Bounds memory against a stream of request models.
const MAX_SCOPES: usize = 64;
const MAX_ROUTE_BYTES: usize = 256;

#[derive(Debug, Default, Clone)]
struct KeyHealth {
    cooling_until: Option<Instant>,
    /// The failure that set `cooling_until`. Stale once the deadline passes, so readers gate on it.
    cooling_class: Option<FailureClass>,
    /// Route-and-model cooldowns; in memory only, never persisted.
    scopes: HashMap<Scope, ScopeHealth>,
    disabled: bool,
    transient_failures: u32,
    last_status: Option<u16>,
    /// Orders `last_status` across keys, so the latest attempt on an upstream can be found.
    status_seq: u64,
}

impl KeyHealth {
    /// Extends a cooldown but never shortens an existing one. Returns true only when the
    /// deadline moves later, so a shorter failure does not emit a second Cooldown event and
    /// does not relabel the cooldown it could not extend.
    fn cool_until(&mut self, until: Instant, class: FailureClass) -> bool {
        match self.cooling_until {
            Some(existing) if existing >= until => false,
            _ => {
                self.cooling_until = Some(until);
                self.cooling_class = Some(class);
                true
            }
        }
    }

    fn scope_cooling_remaining(&self, scope: &Scope, now: Instant) -> Option<Duration> {
        self.scopes
            .get(scope)?
            .cooling_until
            .checked_duration_since(now)
            .filter(|duration| !duration.is_zero())
    }

    fn prune_scopes(&mut self, now: Instant) {
        self.scopes.retain(|_, health| health.cooling_until > now);
    }

    fn cooling_models(&self, now: Instant) -> usize {
        let mut models: Vec<&str> = Vec::new();
        for (scope, health) in &self.scopes {
            if health.cooling_until > now && !models.contains(&scope.model.as_str()) {
                models.push(&scope.model);
            }
        }
        models.len()
    }

    /// Cools one combination, evicting the soonest-ending entry when the map is full.
    /// Returns false when an existing, later deadline already covers this failure.
    fn cool_scope(
        &mut self,
        scope: &Scope,
        until: Instant,
        class: FailureClass,
        now: Instant,
    ) -> bool {
        self.prune_scopes(now);
        if let Some(existing) = self.scopes.get_mut(scope) {
            if existing.cooling_until >= until {
                return false;
            }
            existing.cooling_until = until;
            existing.class = class;
            return true;
        }
        if self.scopes.len() >= MAX_SCOPES {
            if let Some(victim) = self
                .scopes
                .iter()
                .min_by_key(|(_, health)| health.cooling_until)
                .map(|(scope, _)| scope.clone())
            {
                self.scopes.remove(&victim);
            }
        }
        self.scopes.insert(
            scope.clone(),
            ScopeHealth {
                cooling_until: until,
                class,
            },
        );
        true
    }
}

#[derive(Debug, Default)]
pub struct KeyState {
    health: HashMap<String, KeyHealth>,
    status_seq: u64,
}

/// Coarse view of a key's rotation state for operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStatus {
    /// Usable right now.
    Available,
    /// Temporarily out; becomes usable again after the duration.
    Cooling(Duration),
    /// Disabled until an operator re-enables it.
    OutOfRotation,
}

/// A key's health without the monotonic clock, so it can cross a process
/// boundary. The cooldown is a remaining duration that the caller re-anchors to
/// its own clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedHealth {
    pub disabled: bool,
    pub cooling: Option<Duration>,
    pub transient_failures: u32,
}

impl KeyState {
    pub fn is_available(&self, key: &str, now: Instant) -> bool {
        self.is_available_for(key, None, now)
    }

    /// A key is usable when neither the key nor the request's combination is cooling.
    pub fn is_available_for(&self, key: &str, scope: Option<&Scope>, now: Instant) -> bool {
        let Some(h) = self.health.get(key) else {
            return true;
        };
        !h.disabled
            && h.cooling_until.is_none_or(|until| until <= now)
            && scope.is_none_or(|scope| h.scope_cooling_remaining(scope, now).is_none())
    }

    pub fn status(&self, key: &str, now: Instant) -> KeyStatus {
        match self.health.get(key) {
            Some(h) if h.disabled => KeyStatus::OutOfRotation,
            _ => match self.cooling_remaining(key, now) {
                Some(remaining) => KeyStatus::Cooling(remaining),
                None => KeyStatus::Available,
            },
        }
    }

    /// Count of consecutive transient failures since the last success.
    pub fn consecutive_failures(&self, key: &str) -> u32 {
        self.health.get(key).map_or(0, |h| h.transient_failures)
    }

    /// The upstream status of the key's latest attempt, if one arrived.
    pub fn last_status(&self, key: &str) -> Option<u16> {
        self.health.get(key).and_then(|h| h.last_status)
    }

    /// Remember the upstream status of an attempt, including a failed connection
    /// that produced none.
    pub fn record_status(&mut self, key: &str, status: Option<u16>) {
        self.status_seq += 1;
        let health = self.health.entry(key.to_owned()).or_default();
        health.last_status = status;
        health.status_seq = self.status_seq;
    }

    /// The status of the most recent attempt on any of `keys`: `None` when none was tried,
    /// `Some(None)` when that attempt got no response.
    pub fn latest_status<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a str>,
    ) -> Option<Option<u16>> {
        keys.into_iter()
            .filter_map(|key| self.health.get(key))
            .filter(|h| h.status_seq > 0)
            .max_by_key(|h| h.status_seq)
            .map(|h| h.last_status)
    }

    pub fn is_disabled(&self, key: &str) -> bool {
        self.health.get(key).is_some_and(|h| h.disabled)
    }

    pub fn cooling_remaining(&self, key: &str, now: Instant) -> Option<Duration> {
        self.health
            .get(key)?
            .cooling_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    /// The failure class behind the key's current cooldown. `None` when the key is not cooling,
    /// or when the cooldown came from a state file, which does not record a class.
    pub fn cooling_class(&self, key: &str, now: Instant) -> Option<FailureClass> {
        self.cooling_remaining(key, now)?;
        self.health.get(key)?.cooling_class
    }

    /// Every known key's health, for persistence. Cooldowns are expressed as a
    /// remaining duration rather than an instant.
    pub fn snapshot(&self, now: Instant) -> HashMap<String, SavedHealth> {
        self.health
            .iter()
            .map(|(name, h)| {
                (
                    name.clone(),
                    SavedHealth {
                        disabled: h.disabled,
                        cooling: self.cooling_remaining(name, now),
                        transient_failures: h.transient_failures,
                    },
                )
            })
            .collect()
    }

    /// Apply saved health to a key, anchoring the cooldown to `now`.
    pub fn restore(&mut self, key: &str, saved: SavedHealth, now: Instant) {
        self.health.insert(
            key.to_owned(),
            KeyHealth {
                cooling_until: saved.cooling.map(|cooling| now + cooling),
                cooling_class: None,
                scopes: HashMap::new(),
                disabled: saved.disabled,
                transient_failures: saved.transient_failures,
                last_status: None,
                status_seq: 0,
            },
        );
    }

    /// Every cooling combination on a key, soonest first, dropping expired entries.
    pub fn cooling_scopes(&mut self, key: &str, now: Instant) -> Vec<CoolingScope> {
        let Some(h) = self.health.get_mut(key) else {
            return Vec::new();
        };
        h.prune_scopes(now);
        let mut cooling: Vec<CoolingScope> = h
            .scopes
            .iter()
            .filter_map(|(scope, health)| {
                Some(CoolingScope {
                    scope: scope.clone(),
                    remaining: health.cooling_until.checked_duration_since(now)?,
                    class: health.class,
                })
            })
            .collect();
        cooling.sort_by(|a, b| {
            a.remaining
                .cmp(&b.remaining)
                .then_with(|| a.scope.route.cmp(&b.scope.route))
                .then_with(|| a.scope.model.cmp(&b.scope.model))
        });
        cooling
    }

    /// Record a key-level failure. Returns true when the key is now out of rotation.
    pub fn record_failure(
        &mut self,
        key: &str,
        class: FailureClass,
        policy: &Policy,
        now: Instant,
    ) -> bool {
        self.record_failure_with(key, class, policy, now, None)
    }

    /// Like `record_failure`, but a quota-exhausted failure cools for
    /// `quota_cooldown` when the caller derived one from a reset schedule; when
    /// it is `None` the policy's fixed window applies.
    pub fn record_failure_with(
        &mut self,
        key: &str,
        class: FailureClass,
        policy: &Policy,
        now: Instant,
        quota_cooldown: Option<Duration>,
    ) -> bool {
        self.record_failure_scoped(key, class, policy, now, quota_cooldown, None, 0)
            .is_some()
    }

    /// Records a failure, on `scope` when the caller says it belongs to a combination, and
    /// otherwise on the key. After `escalate_after` models are cooling on one key, or the
    /// failure brings the cooling combinations to twice that, the key cools instead. Returns
    /// what was cooled, if anything.
    #[allow(clippy::too_many_arguments)]
    pub fn record_failure_scoped(
        &mut self,
        key: &str,
        class: FailureClass,
        policy: &Policy,
        now: Instant,
        quota_cooldown: Option<Duration>,
        scope: Option<&Scope>,
        escalate_after: u32,
    ) -> Option<CooldownScope> {
        let h = self.health.entry(key.to_owned()).or_default();
        match class {
            FailureClass::Unauthorized => {
                h.disabled = true;
                Some(CooldownScope::Key)
            }
            FailureClass::RateLimited(retry_after) => {
                let until = now + retry_after.unwrap_or(policy.rate_limited_default);
                Self::cool_failure(h, scope, until, class, now, escalate_after)
            }
            FailureClass::QuotaExhausted => {
                let until = now + quota_cooldown.unwrap_or(policy.quota_exhausted);
                Self::cool_failure(h, scope, until, class, now, escalate_after)
            }
            // Request-scoped: the proxy skips the key for this request and never records it.
            FailureClass::AccessDisabled => None,
            FailureClass::Transient => {
                h.transient_failures += 1;
                if h.transient_failures >= policy.transient_threshold {
                    h.transient_failures = 0;
                    h.cool_until(now + policy.transient, class)
                        .then_some(CooldownScope::Key)
                } else {
                    None
                }
            }
        }
    }

    fn cool_failure(
        h: &mut KeyHealth,
        scope: Option<&Scope>,
        until: Instant,
        class: FailureClass,
        now: Instant,
        escalate_after: u32,
    ) -> Option<CooldownScope> {
        // A key cooldown covers every combination, so no scoped entry outlives it.
        if h.cooling_until.is_some_and(|deadline| deadline > now) {
            return Self::cool_key(h, until, class);
        }
        let Some(scope) = scope else {
            return Self::cool_key(h, until, class);
        };
        h.prune_scopes(now);
        let models = h.cooling_models(now);
        let combinations = h.scopes.len();
        let would_reach = combinations.saturating_add(usize::from(!h.scopes.contains_key(scope)));
        let combination_limit = (escalate_after as usize).saturating_mul(2);
        if escalate_after >= 2
            && (models >= escalate_after as usize || would_reach >= combination_limit)
        {
            return Self::cool_key(h, until, class);
        }
        h.cool_scope(scope, until, class, now)
            .then_some(CooldownScope::Model)
    }

    fn cool_key(h: &mut KeyHealth, until: Instant, class: FailureClass) -> Option<CooldownScope> {
        h.scopes.clear();
        h.cool_until(until, class).then_some(CooldownScope::Key)
    }

    pub fn record_success(&mut self, key: &str) {
        if let Some(h) = self.health.get_mut(key) {
            h.transient_failures = 0;
        }
    }

    pub fn disable(&mut self, key: &str) {
        self.health.entry(key.to_owned()).or_default().disabled = true;
    }

    pub fn enable(&mut self, key: &str) {
        if let Some(h) = self.health.get_mut(key) {
            *h = KeyHealth::default();
        }
    }

    pub fn retain_keys(&mut self, keep: impl Fn(&str) -> bool) {
        self.health.retain(|name, _| keep(name));
    }

    /// Time until the first accepted key of the chain becomes usable again. `accept` must
    /// match the keys the caller would pick, or a healthy key elsewhere reports no wait.
    /// `None` when every accepted key is disabled, which has no recovery time.
    pub fn soonest_recovery(
        &self,
        chain: &[String],
        now: Instant,
        accept: impl Fn(&str) -> bool,
    ) -> Option<Duration> {
        self.soonest_recovery_for(chain, None, now, accept)
    }

    /// Like `soonest_recovery`, but a key also waits for the request's combination cooldown.
    pub fn soonest_recovery_for(
        &self,
        chain: &[String],
        scope: Option<&Scope>,
        now: Instant,
        accept: impl Fn(&str) -> bool,
    ) -> Option<Duration> {
        chain
            .iter()
            .filter(|k| accept(k.as_str()) && !self.is_disabled(k))
            .map(|k| {
                let key_wait = self.cooling_remaining(k, now).unwrap_or(Duration::ZERO);
                let scope_wait = scope
                    .and_then(|scope| self.health.get(k)?.scope_cooling_remaining(scope, now))
                    .unwrap_or(Duration::ZERO);
                key_wait.max(scope_wait)
            })
            .min()
    }
}

#[derive(Debug)]
pub struct Chain {
    keys: Vec<String>,
    strategy: Strategy,
    cursor: usize,
}

impl Chain {
    pub fn new(keys: Vec<String>, strategy: Strategy) -> Self {
        Self {
            keys,
            strategy,
            cursor: 0,
        }
    }

    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// The key this chain would prefer next, ignoring health. Read-only companion to `pick`.
    pub fn current(&self) -> Option<&str> {
        let n = self.keys.len();
        if n == 0 {
            return None;
        }
        let index = match self.strategy {
            Strategy::Sticky => self.cursor % n,
            Strategy::RoundRobin => (self.cursor + 1) % n,
        };
        self.keys.get(index).map(String::as_str)
    }

    pub fn pick(&mut self, state: &KeyState, now: Instant) -> Option<&str> {
        self.pick_scoped(state, now, None)
    }

    pub fn pick_scoped(
        &mut self,
        state: &KeyState,
        now: Instant,
        scope: Option<&Scope>,
    ) -> Option<&str> {
        self.pick_where_scoped(state, now, scope, |_| true)
    }

    pub fn pick_where(
        &mut self,
        state: &KeyState,
        now: Instant,
        accept: impl Fn(&str) -> bool,
    ) -> Option<&str> {
        self.pick_where_scoped(state, now, None, accept)
    }

    pub fn pick_where_scoped(
        &mut self,
        state: &KeyState,
        now: Instant,
        scope: Option<&Scope>,
        accept: impl Fn(&str) -> bool,
    ) -> Option<&str> {
        let n = self.keys.len();
        if n == 0 {
            return None;
        }
        let start = match self.strategy {
            Strategy::Sticky => self.cursor % n,
            Strategy::RoundRobin => (self.cursor + 1) % n,
        };
        let found = (0..n).map(|offset| (start + offset) % n).find(|&i| {
            accept(&self.keys[i]) && state.is_available_for(&self.keys[i], scope, now)
        })?;
        self.cursor = found;
        Some(&self.keys[found])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_shorter_failure_does_not_relabel_a_quota_cooldown() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::QuotaExhausted, &policy, now);
        state.record_failure(
            "a",
            FailureClass::RateLimited(Some(secs(20))),
            &policy,
            now + secs(1),
        );
        assert_eq!(
            state.cooling_class("a", now + secs(1)),
            Some(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn a_longer_failure_takes_over_the_cooling_class() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::RateLimited(Some(secs(30))), &policy, now);
        state.record_failure("a", FailureClass::QuotaExhausted, &policy, now + secs(1));
        assert_eq!(
            state.cooling_class("a", now + secs(1)),
            Some(FailureClass::QuotaExhausted)
        );
    }

    #[test]
    fn a_transient_only_cooldown_reports_transient() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::Transient, &policy, now);
        assert_eq!(state.cooling_class("a", now), None, "not cooling yet");
        state.record_failure("a", FailureClass::Transient, &policy, now);
        assert_eq!(state.cooling_class("a", now), Some(FailureClass::Transient));
    }

    #[test]
    fn the_cooling_class_ends_with_the_cooldown() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::QuotaExhausted, &policy, now);
        assert_eq!(
            state.cooling_class("a", now + policy.quota_exhausted - secs(1)),
            Some(FailureClass::QuotaExhausted)
        );
        assert_eq!(state.cooling_class("a", now + policy.quota_exhausted), None);
    }

    #[test]
    fn enabling_a_key_clears_its_cooling_class() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::QuotaExhausted, &policy, now);
        state.enable("a");
        assert_eq!(state.cooling_class("a", now), None);
        state.record_failure("a", FailureClass::RateLimited(Some(secs(10))), &policy, now);
        assert_eq!(
            state.cooling_class("a", now),
            Some(FailureClass::RateLimited(Some(secs(10))))
        );
    }

    #[test]
    fn a_restored_cooldown_has_no_class() {
        let mut state = KeyState::default();
        let now = Instant::now();
        state.record_failure("a", FailureClass::QuotaExhausted, &Policy::default(), now);
        let saved = state.snapshot(now);
        let mut restored = KeyState::default();
        restored.restore("a", saved["a"], now);
        assert_eq!(
            restored.cooling_remaining("a", now),
            Some(Policy::default().quota_exhausted)
        );
        assert_eq!(restored.cooling_class("a", now), None);
    }

    #[test]
    fn the_latest_status_is_the_most_recent_attempt_across_keys() {
        let mut state = KeyState::default();
        assert_eq!(state.latest_status(["a", "b"]), None);
        state.record_status("a", Some(429));
        state.record_status("b", Some(200));
        assert_eq!(state.latest_status(["a", "b"]), Some(Some(200)));
        state.record_status("a", None);
        assert_eq!(state.latest_status(["a", "b"]), Some(None));
        assert_eq!(state.latest_status(["b"]), Some(Some(200)));
        assert_eq!(state.latest_status(["c"]), None);
    }

    #[test]
    fn unknown_keys_are_available() {
        let state = KeyState::default();
        assert!(state.is_available("a", Instant::now()));
    }

    #[test]
    fn rate_limit_cools_for_retry_after_then_recovers() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        assert!(state.record_failure("a", FailureClass::RateLimited(Some(secs(10))), &policy, now));
        assert!(!state.is_available("a", now + secs(9)));
        assert_eq!(state.cooling_remaining("a", now + secs(4)), Some(secs(6)));
        assert!(state.is_available("a", now + secs(10)));
    }

    #[test]
    fn a_later_shorter_failure_does_not_shorten_a_quota_cooldown() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        assert!(state.record_failure("a", FailureClass::QuotaExhausted, &policy, now));
        assert!(
            !state.record_failure(
                "a",
                FailureClass::RateLimited(Some(secs(30))),
                &policy,
                now + secs(1)
            ),
            "the deadline did not move later"
        );
        assert_eq!(
            state.cooling_remaining("a", now + secs(1)),
            Some(policy.quota_exhausted - secs(1))
        );
        assert!(
            !state.is_available("a", now + secs(31)),
            "the 30 s rate limit must not replace the 5 h quota window"
        );
        assert!(state.is_available("a", now + policy.quota_exhausted));
    }

    #[test]
    fn a_later_longer_failure_extends_a_rate_limit_cooldown() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        assert!(state.record_failure("a", FailureClass::RateLimited(Some(secs(30))), &policy, now));
        assert!(
            state.record_failure("a", FailureClass::QuotaExhausted, &policy, now + secs(1)),
            "the quota deadline is later"
        );
        assert_eq!(
            state.cooling_remaining("a", now + secs(1)),
            Some(policy.quota_exhausted)
        );
    }

    #[test]
    fn a_quota_reset_cooldown_overrides_the_fixed_window() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        assert!(state.record_failure_with(
            "a",
            FailureClass::QuotaExhausted,
            &policy,
            now,
            Some(secs(5))
        ));
        assert_eq!(state.cooling_remaining("a", now), Some(secs(5)));
        assert!(!state.is_available("a", now + secs(4)));
        assert!(state.is_available("a", now + secs(5)));
    }

    #[test]
    fn a_cleared_quota_cooldown_falls_back_to_the_policy() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure_with("a", FailureClass::QuotaExhausted, &policy, now, None);
        assert_eq!(
            state.cooling_remaining("a", now),
            Some(policy.quota_exhausted)
        );
    }

    #[test]
    fn access_disabled_never_cools_a_key() {
        let mut state = KeyState::default();
        let now = Instant::now();
        assert!(!state.record_failure("a", FailureClass::AccessDisabled, &Policy::default(), now));
        assert!(state.is_available("a", now));
    }

    #[test]
    fn rate_limit_without_header_uses_the_default() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::RateLimited(None), &policy, now);
        assert!(!state.is_available("a", now + policy.rate_limited_default - secs(1)));
        assert!(state.is_available("a", now + policy.rate_limited_default));
    }

    #[test]
    fn quota_failures_use_a_long_cooldown() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("q", FailureClass::QuotaExhausted, &policy, now);
        assert!(!state.is_available("q", now + secs(3600)));
        assert!(state.is_available("q", now + policy.quota_exhausted));
    }

    #[test]
    fn unauthorized_disables_until_an_operator_enables_it() {
        let mut state = KeyState::default();
        let now = Instant::now();
        state.record_failure("a", FailureClass::Unauthorized, &Policy::default(), now);
        assert!(!state.is_available("a", now + secs(100_000)));
        state.enable("a");
        assert!(state.is_available("a", now));
    }

    #[test]
    fn transient_failures_only_cool_after_the_threshold_and_success_resets() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        assert!(!state.record_failure("a", FailureClass::Transient, &policy, now));
        state.record_success("a");
        assert!(!state.record_failure("a", FailureClass::Transient, &policy, now));
        assert!(state.record_failure("a", FailureClass::Transient, &policy, now));
        assert!(!state.is_available("a", now + policy.transient - secs(1)));
        assert!(state.is_available("a", now + policy.transient));
    }

    #[test]
    fn sticky_stays_on_a_key_until_it_fails() {
        let mut state = KeyState::default();
        let mut chain = Chain::new(keys(&["a", "b", "c"]), Strategy::Sticky);
        let now = Instant::now();
        assert_eq!(chain.pick(&state, now), Some("a"));
        assert_eq!(chain.pick(&state, now), Some("a"));
        state.record_failure("a", FailureClass::QuotaExhausted, &Policy::default(), now);
        assert_eq!(chain.pick(&state, now), Some("b"));
        assert_eq!(chain.pick(&state, now), Some("b"));
    }

    #[test]
    fn sticky_wraps_to_an_earlier_key_when_later_ones_are_out() {
        let mut state = KeyState::default();
        let mut chain = Chain::new(keys(&["a", "b"]), Strategy::Sticky);
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::QuotaExhausted, &policy, now);
        assert_eq!(chain.pick(&state, now), Some("b"));
        state.record_failure("b", FailureClass::RateLimited(Some(secs(1))), &policy, now);
        assert_eq!(chain.pick(&state, now), None);
        assert_eq!(chain.pick(&state, now + secs(2)), Some("b"));
    }

    #[test]
    fn round_robin_rotates_and_skips_cooling_keys() {
        let mut state = KeyState::default();
        let mut chain = Chain::new(keys(&["a", "b", "c"]), Strategy::RoundRobin);
        let now = Instant::now();
        let picks: Vec<_> = (0..4)
            .map(|_| chain.pick(&state, now).unwrap().to_owned())
            .collect();
        assert_eq!(picks, ["b", "c", "a", "b"]);
        state.record_failure("c", FailureClass::QuotaExhausted, &Policy::default(), now);
        let picks: Vec<_> = (0..4)
            .map(|_| chain.pick(&state, now).unwrap().to_owned())
            .collect();
        assert_eq!(picks, ["a", "b", "a", "b"]);
    }

    #[test]
    fn streams_sharing_a_key_share_its_cooldown() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let mut one = Chain::new(keys(&["shared", "own1"]), Strategy::Sticky);
        let mut two = Chain::new(keys(&["shared", "own2"]), Strategy::Sticky);
        assert_eq!(one.pick(&state, now), Some("shared"));
        state.record_failure(
            "shared",
            FailureClass::QuotaExhausted,
            &Policy::default(),
            now,
        );
        assert_eq!(one.pick(&state, now), Some("own1"));
        assert_eq!(two.pick(&state, now), Some("own2"));
    }

    #[test]
    fn disjoint_chains_do_not_affect_each_other() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let mut one = Chain::new(keys(&["a1", "a2"]), Strategy::Sticky);
        let mut two = Chain::new(keys(&["b1", "b2"]), Strategy::Sticky);
        state.record_failure("a1", FailureClass::QuotaExhausted, &Policy::default(), now);
        assert_eq!(one.pick(&state, now), Some("a2"));
        assert_eq!(two.pick(&state, now), Some("b1"));
    }

    #[test]
    fn soonest_recovery_ignores_disabled_keys() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        let chain = keys(&["a", "b", "c"]);
        state.record_failure("a", FailureClass::RateLimited(Some(secs(40))), &policy, now);
        state.record_failure("b", FailureClass::RateLimited(Some(secs(15))), &policy, now);
        state.record_failure("c", FailureClass::Unauthorized, &policy, now);
        assert_eq!(
            state.soonest_recovery(&chain, now, |_| true),
            Some(secs(15))
        );
        assert_eq!(
            state.soonest_recovery(&chain, now, |key| key == "a"),
            Some(secs(40)),
            "keys the caller would not pick do not count"
        );
        state.disable("a");
        state.disable("b");
        assert_eq!(state.soonest_recovery(&chain, now, |_| true), None);
    }

    #[test]
    fn pick_where_only_considers_accepted_keys() {
        let state = KeyState::default();
        let mut chain = Chain::new(keys(&["zen-a", "other-a", "zen-b"]), Strategy::Sticky);
        let now = Instant::now();
        assert_eq!(
            chain.pick_where(&state, now, |k| k.starts_with("other")),
            Some("other-a")
        );
        assert_eq!(
            chain.pick_where(&state, now, |k| k == "zen-b"),
            Some("zen-b")
        );
        assert_eq!(chain.pick_where(&state, now, |k| k == "missing"), None);
    }

    #[test]
    fn an_empty_chain_picks_nothing() {
        let mut chain = Chain::new(vec![], Strategy::Sticky);
        assert_eq!(chain.pick(&KeyState::default(), Instant::now()), None);
    }

    #[test]
    fn retain_keys_keeps_known_health_and_drops_the_rest() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("keep", FailureClass::Unauthorized, &policy, now);
        state.record_failure("gone", FailureClass::Unauthorized, &policy, now);
        state.retain_keys(|name| name == "keep");
        assert!(!state.is_available("keep", now));
        assert!(state.is_available("gone", now));
    }

    #[test]
    fn status_reports_availability_cooling_and_rotation() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        assert_eq!(state.status("fresh", now), KeyStatus::Available);
        state.record_failure(
            "cool",
            FailureClass::RateLimited(Some(secs(10))),
            &policy,
            now,
        );
        assert_eq!(
            state.status("cool", now + secs(4)),
            KeyStatus::Cooling(secs(6))
        );
        assert_eq!(state.status("cool", now + secs(10)), KeyStatus::Available);
        state.record_failure("dead", FailureClass::Unauthorized, &policy, now);
        assert_eq!(state.status("dead", now), KeyStatus::OutOfRotation);
    }

    #[test]
    fn consecutive_failures_tracks_transient_strikes() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("a", FailureClass::Transient, &policy, now);
        assert_eq!(state.consecutive_failures("a"), 1);
        state.record_failure("a", FailureClass::RateLimited(None), &policy, now);
        assert_eq!(state.consecutive_failures("a"), 1);
        state.record_success("a");
        assert_eq!(state.consecutive_failures("a"), 0);
        assert_eq!(state.consecutive_failures("unknown"), 0);
    }

    #[test]
    fn current_reflects_the_strategy_without_mutating() {
        let sticky = Chain::new(keys(&["a", "b"]), Strategy::Sticky);
        assert_eq!(sticky.current(), Some("a"));
        let round_robin = Chain::new(keys(&["a", "b"]), Strategy::RoundRobin);
        assert_eq!(round_robin.current(), Some("b"));
        let empty = Chain::new(Vec::new(), Strategy::Sticky);
        assert_eq!(empty.current(), None);
    }

    #[test]
    fn snapshot_and_restore_round_trip_key_health() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure(
            "cool",
            FailureClass::RateLimited(Some(secs(40))),
            &policy,
            now,
        );
        state.record_failure("dead", FailureClass::Unauthorized, &policy, now);

        let saved = state.snapshot(now);
        let mut restored = KeyState::default();
        for (name, health) in &saved {
            restored.restore(name, *health, now);
        }
        assert_eq!(restored.cooling_remaining("cool", now), Some(secs(40)));
        assert!(!restored.is_available("dead", now + secs(100_000)));
    }

    #[test]
    fn the_last_status_tracks_the_latest_attempt() {
        let mut state = KeyState::default();
        assert_eq!(state.last_status("a"), None);
        state.record_status("a", Some(429));
        assert_eq!(state.last_status("a"), Some(429));
        state.record_status("a", None);
        assert_eq!(state.last_status("a"), None);
    }

    #[test]
    fn restore_preserves_transient_strikes() {
        let mut state = KeyState::default();
        let now = Instant::now();
        state.record_failure("a", FailureClass::Transient, &Policy::default(), now);
        let saved = state.snapshot(now);
        let mut restored = KeyState::default();
        restored.restore("a", saved["a"], now);
        assert_eq!(restored.consecutive_failures("a"), 1);
    }

    #[test]
    fn scope_routes_drop_the_query_and_collapse_id_like_segments() {
        assert_eq!(scope_route("/responses?stream=true"), "/responses");
        assert_eq!(scope_route("/chat/completions"), "/chat/completions");
        assert_eq!(
            scope_route("/files/123/download/abcdefghijklmnopqrstuvwxyz"),
            "/files/*/download/*"
        );
        assert_eq!(scope_route(""), "");
        let long = format!("/{}", "é/".repeat(300));
        assert!(scope_route(&long).len() <= MAX_ROUTE_BYTES);
        assert!(scope_route(&long).starts_with("/é/é"));
    }

    fn scope(route: &str, model: &str) -> Scope {
        Scope {
            route: route.to_owned(),
            model: model.to_owned(),
        }
    }

    #[test]
    fn a_combination_failure_cools_only_that_route_and_model() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        let paid = scope("/responses", "gpt-big");
        let free = scope("/chat/completions", "free-model");

        assert_eq!(
            state.record_failure_scoped(
                "a",
                FailureClass::RateLimited(Some(secs(10))),
                &policy,
                now,
                None,
                Some(&paid),
                3,
            ),
            Some(CooldownScope::Model)
        );
        assert!(state.is_available("a", now), "the key itself stays usable");
        assert!(!state.is_available_for("a", Some(&paid), now));
        assert!(state.is_available_for("a", Some(&free), now));
        assert!(state.is_available_for("a", None, now));

        let cooling = state.cooling_scopes("a", now);
        assert_eq!(cooling.len(), 1);
        assert_eq!(cooling[0].scope, paid);
        assert_eq!(cooling[0].remaining, secs(10));
        assert_eq!(cooling[0].class, FailureClass::RateLimited(Some(secs(10))));
        assert!(state.cooling_scopes("a", now + secs(10)).is_empty());
    }

    #[test]
    fn enough_distinct_cooling_models_escalate_to_the_key() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        for model in ["m1", "m2", "m3"] {
            assert_eq!(
                state.record_failure_scoped(
                    "a",
                    FailureClass::RateLimited(Some(secs(10))),
                    &policy,
                    now,
                    None,
                    Some(&scope("/chat", model)),
                    3,
                ),
                Some(CooldownScope::Model),
                "{model} is still a combination cooldown"
            );
        }
        assert!(
            state.is_available("a", now),
            "three models cool, the key is still usable"
        );
        assert_eq!(
            state.record_failure_scoped(
                "a",
                FailureClass::RateLimited(Some(secs(10))),
                &policy,
                now,
                None,
                Some(&scope("/chat", "m4")),
                3,
            ),
            Some(CooldownScope::Key),
            "the fourth combination failure escalates"
        );
        assert!(!state.is_available("a", now));
    }

    #[test]
    fn many_paths_on_one_model_escalate_at_the_combination_threshold() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        let cooldown = secs(10);
        for index in 0..5u64 {
            assert_eq!(
                state.record_failure_scoped(
                    "a",
                    FailureClass::RateLimited(Some(cooldown)),
                    &policy,
                    now,
                    None,
                    Some(&scope(&format!("/chat/path-{index}"), "one-model")),
                    3,
                ),
                Some(CooldownScope::Model),
                "path {index} is below the combination threshold"
            );
        }
        assert!(
            state.is_available("a", now),
            "five combinations cool, the key is still usable"
        );
        assert_eq!(
            state.record_failure_scoped(
                "a",
                FailureClass::RateLimited(Some(cooldown)),
                &policy,
                now,
                None,
                Some(&scope("/chat/path-5", "one-model")),
                3,
            ),
            Some(CooldownScope::Key),
            "the sixth combination reaches 2 * 3 and cools the key"
        );
        assert!(!state.is_available("a", now));
        assert!(
            state.cooling_scopes("a", now).is_empty(),
            "the key cooldown replaces the combination entries"
        );
        assert!(state.cooling_scopes("a", now + cooldown).is_empty());
        assert!(state.is_available("a", now + cooldown));
    }

    #[test]
    fn the_combination_map_is_bounded_and_evicts_the_soonest_ending() {
        // Straight at the map: escalation would otherwise cool the key long before 64 models.
        let mut health = KeyHealth::default();
        let now = Instant::now();
        for index in 0..1_000u64 {
            health.cool_scope(
                &scope("/chat", &format!("model-{index}")),
                now + secs(20 + index % 40),
                FailureClass::RateLimited(None),
                now,
            );
        }
        assert_eq!(health.scopes.len(), MAX_SCOPES);
        assert!(health
            .scopes
            .values()
            .all(|h| h.cooling_until >= now + secs(20)));
    }

    #[test]
    fn many_distinct_models_escalate_to_the_key_instead_of_filling_the_map() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        for index in 0..1_000u64 {
            state.record_failure_scoped(
                "a",
                FailureClass::RateLimited(Some(secs(30))),
                &policy,
                now,
                None,
                Some(&scope("/chat", &format!("model-{index}"))),
                16,
            );
        }
        assert!(matches!(state.status("a", now), KeyStatus::Cooling(_)));
        assert!(state.health["a"].scopes.len() <= 16);
    }
}
