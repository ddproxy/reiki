use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::classify::FailureClass;

#[derive(Debug, Clone)]
pub struct Policy {
    pub rate_limited_default: Duration,
    pub quota_exhausted: Duration,
    pub access_disabled: Duration,
    pub transient: Duration,
    pub transient_threshold: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            rate_limited_default: Duration::from_secs(60),
            quota_exhausted: Duration::from_secs(5 * 3600),
            access_disabled: Duration::from_secs(5 * 3600),
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

#[derive(Debug, Default, Clone)]
struct KeyHealth {
    cooling_until: Option<Instant>,
    disabled: bool,
    transient_failures: u32,
}

#[derive(Debug, Default)]
pub struct KeyState {
    health: HashMap<String, KeyHealth>,
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
        match self.health.get(key) {
            None => true,
            Some(h) => !h.disabled && h.cooling_until.is_none_or(|until| until <= now),
        }
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
                disabled: saved.disabled,
                transient_failures: saved.transient_failures,
            },
        );
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
        let h = self.health.entry(key.to_owned()).or_default();
        match class {
            FailureClass::Unauthorized => {
                h.disabled = true;
                true
            }
            FailureClass::RateLimited(retry_after) => {
                h.cooling_until = Some(now + retry_after.unwrap_or(policy.rate_limited_default));
                true
            }
            FailureClass::QuotaExhausted => {
                h.cooling_until = Some(now + quota_cooldown.unwrap_or(policy.quota_exhausted));
                true
            }
            FailureClass::AccessDisabled => {
                h.cooling_until = Some(now + policy.access_disabled);
                true
            }
            FailureClass::Transient => {
                h.transient_failures += 1;
                if h.transient_failures >= policy.transient_threshold {
                    h.cooling_until = Some(now + policy.transient);
                    h.transient_failures = 0;
                    true
                } else {
                    false
                }
            }
        }
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

    /// Time until the first key of the chain becomes usable again. `None` when every key
    /// is disabled, which has no recovery time.
    pub fn soonest_recovery(&self, chain: &[String], now: Instant) -> Option<Duration> {
        chain
            .iter()
            .filter(|k| !self.is_disabled(k))
            .map(|k| self.cooling_remaining(k, now).unwrap_or(Duration::ZERO))
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
        self.pick_where(state, now, |_| true)
    }

    pub fn pick_where(
        &mut self,
        state: &KeyState,
        now: Instant,
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
        let found = (0..n)
            .map(|offset| (start + offset) % n)
            .find(|&i| accept(&self.keys[i]) && state.is_available(&self.keys[i], now))?;
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
    fn a_quota_cooldown_does_not_change_other_classes() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure_with(
            "a",
            FailureClass::AccessDisabled,
            &policy,
            now,
            Some(secs(5)),
        );
        assert_eq!(
            state.cooling_remaining("a", now),
            Some(policy.access_disabled)
        );
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
    fn quota_and_access_failures_use_long_cooldowns() {
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure("q", FailureClass::QuotaExhausted, &policy, now);
        state.record_failure("d", FailureClass::AccessDisabled, &policy, now);
        assert!(!state.is_available("q", now + secs(3600)));
        assert!(!state.is_available("d", now + secs(3600)));
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
        assert_eq!(state.soonest_recovery(&chain, now), Some(secs(15)));
        state.disable("a");
        state.disable("b");
        assert_eq!(state.soonest_recovery(&chain, now), None);
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
    fn restore_preserves_transient_strikes() {
        let mut state = KeyState::default();
        let now = Instant::now();
        state.record_failure("a", FailureClass::Transient, &Policy::default(), now);
        let saved = state.snapshot(now);
        let mut restored = KeyState::default();
        restored.restore("a", saved["a"], now);
        assert_eq!(restored.consecutive_failures("a"), 1);
    }
}
