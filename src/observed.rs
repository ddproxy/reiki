//! What passthrough upstreams reported, in memory only.
//!
//! A passthrough route holds no Reiki key. Reiki still watches the answers it relays and records
//! the response class, the upstream's own retry hint and the quota numbers its headers carry. The
//! store is keyed by `(upstream, account)`: the account is an operator label from config, never
//! derived from a credential, because credentials rotate. No credential value is ever held here.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::classify::{FailureClass, Outcome};
use crate::shaper::headers::Observed as ObservedQuota;

/// The account label used when an upstream does not set one.
pub(crate) const DEFAULT_ACCOUNT: &str = "default";

pub(crate) const OK: &str = "ok";
pub(crate) const RATE_LIMITED: &str = "rate_limited";
pub(crate) const QUOTA_EXHAUSTED: &str = "quota_exhausted";
pub(crate) const CREDENTIAL_REJECTED: &str = "credential_rejected";
pub(crate) const ACCESS_DISABLED: &str = "access_disabled";
pub(crate) const REQUEST_ERROR: &str = "request_error";
pub(crate) const ERROR: &str = "error";
pub(crate) const PASS_THROUGH: &str = "pass_through";

/// The class vocabulary is fixed here, never taken from a response body or a header.
pub(crate) fn status_class(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Success => OK,
        Outcome::KeyFailure(FailureClass::RateLimited(_)) => RATE_LIMITED,
        Outcome::KeyFailure(FailureClass::QuotaExhausted) => QUOTA_EXHAUSTED,
        Outcome::KeyFailure(FailureClass::Unauthorized) => CREDENTIAL_REJECTED,
        Outcome::KeyFailure(FailureClass::AccessDisabled) => ACCESS_DISABLED,
        Outcome::KeyFailure(FailureClass::Transient) => ERROR,
        Outcome::RequestError => REQUEST_ERROR,
        Outcome::PassThrough => PASS_THROUGH,
    }
}

/// The informational cooldown class, or `None` when the answer did not start one.
pub(crate) fn cooldown_class(outcome: Outcome) -> Option<&'static str> {
    match outcome {
        Outcome::KeyFailure(class) => Some(class.name()),
        _ => None,
    }
}

/// Quota numbers read from response headers. Durations become whole seconds, rounded up so a
/// sub-second wait never reads as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct QuotaSnapshot {
    pub(crate) requests_limit: Option<u64>,
    pub(crate) requests_remaining: Option<u64>,
    pub(crate) requests_reset_seconds: Option<u64>,
    pub(crate) tokens_limit: Option<u64>,
    pub(crate) tokens_remaining: Option<u64>,
    pub(crate) tokens_reset_seconds: Option<u64>,
}

impl From<ObservedQuota> for QuotaSnapshot {
    fn from(quota: ObservedQuota) -> Self {
        let secs = |wait: Option<Duration>| {
            wait.map(|wait| wait.as_secs() + u64::from(wait.subsec_nanos() > 0))
        };
        Self {
            requests_limit: quota.request_limit,
            requests_remaining: quota.requests_remaining,
            requests_reset_seconds: secs(quota.requests_reset),
            tokens_limit: quota.token_limit,
            tokens_remaining: quota.tokens_remaining,
            tokens_reset_seconds: secs(quota.tokens_reset),
        }
    }
}

/// One published observed entry. `retry_after_seconds` is the remaining wait when read, not a
/// value copied from the upstream's own (possibly skewed) clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ObservedEntry {
    pub(crate) upstream: String,
    pub(crate) account: String,
    pub(crate) mode: &'static str,
    pub(crate) status_class: &'static str,
    pub(crate) observed_at: String,
    pub(crate) retry_after_seconds: Option<u64>,
    pub(crate) cooldown_class: Option<&'static str>,
    pub(crate) quota: QuotaSnapshot,
}

impl From<&ObservedEntry> for reiki_health::ObservedUpstream {
    fn from(entry: &ObservedEntry) -> Self {
        Self {
            upstream: entry.upstream.clone(),
            account: entry.account.clone(),
            mode: entry.mode.to_owned(),
            status_class: entry.status_class.to_owned(),
            observed_at: entry.observed_at.clone(),
            retry_after_seconds: entry.retry_after_seconds,
            cooldown_class: entry.cooldown_class.map(str::to_owned),
            quota: reiki_health::ObservedQuota {
                requests_limit: entry.quota.requests_limit,
                requests_remaining: entry.quota.requests_remaining,
                requests_reset_seconds: entry.quota.requests_reset_seconds,
                tokens_limit: entry.quota.tokens_limit,
                tokens_remaining: entry.quota.tokens_remaining,
                tokens_reset_seconds: entry.quota.tokens_reset_seconds,
            },
        }
    }
}

#[derive(Debug)]
struct Stored {
    entry: ObservedEntry,
    retry_after_until: Option<SystemTime>,
}

/// Bounded by configured upstreams and their one account label, never by request volume. A full
/// store keeps the entries it already has: a new account label cannot displace an existing one.
const MAX_ENTRIES: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct ObservedStore {
    entries: Mutex<BTreeMap<(String, String), Stored>>,
}

impl ObservedStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records one passthrough response. The retry hint is already parsed and capped in `quota`;
    /// `now` is the host's wall clock, so the published timestamp never comes from the upstream.
    pub(crate) fn record(
        &self,
        upstream: &str,
        account: &str,
        class: &'static str,
        cooldown: Option<&'static str>,
        quota: ObservedQuota,
        now: SystemTime,
    ) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (upstream.to_owned(), account.to_owned());
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(&key) {
            return;
        }
        let retry_after_until = quota.retry_after.and_then(|wait| now.checked_add(wait));
        let remaining = retry_after_until.map(|until| {
            until.duration_since(now).map_or(0, |wait| {
                wait.as_secs() + u64::from(wait.subsec_nanos() > 0)
            })
        });
        let entry = ObservedEntry {
            upstream: upstream.to_owned(),
            account: account.to_owned(),
            mode: "observed",
            status_class: class,
            observed_at: crate::reload::rfc3339(now),
            retry_after_seconds: remaining,
            cooldown_class: cooldown,
            quota: quota.into(),
        };
        entries.insert(
            key,
            Stored {
                entry,
                retry_after_until,
            },
        );
    }

    /// All current entries, in a stable order for `/v1/status` and the peers document.
    pub(crate) fn snapshot(&self) -> Vec<ObservedEntry> {
        self.snapshot_at(SystemTime::now())
    }

    /// All current entries with each retry hint counted down to `now`.
    pub(crate) fn snapshot_at(&self, now: SystemTime) -> Vec<ObservedEntry> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .values()
            .map(|stored| {
                let mut entry = stored.entry.clone();
                entry.retry_after_seconds = stored.retry_after_until.and_then(|until| {
                    until
                        .duration_since(now)
                        .ok()
                        .map(|wait| wait.as_secs() + u64::from(wait.subsec_nanos() > 0))
                });
                entry
            })
            .collect()
    }

    /// Every account still cooling, sampled once for the admin metric. The account is kept for a
    /// plugin's own report but is not used as a metric label.
    pub(crate) fn cooldowns(&self, now: SystemTime) -> Vec<reiki_plugin::ObservedCooldown> {
        self.snapshot_at(now)
            .into_iter()
            .filter_map(|entry| {
                let seconds = entry.retry_after_seconds?;
                Some(reiki_plugin::ObservedCooldown {
                    upstream: entry.upstream,
                    account: entry.account,
                    class: entry.cooldown_class?.to_owned(),
                    seconds_remaining: seconds,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn record_replaces_the_entry_for_the_same_upstream_and_account() {
        let store = ObservedStore::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        store.record(
            "oauth",
            DEFAULT_ACCOUNT,
            OK,
            None,
            ObservedQuota::default(),
            now,
        );
        store.record(
            "oauth",
            DEFAULT_ACCOUNT,
            RATE_LIMITED,
            Some("rate_limited"),
            ObservedQuota {
                requests_remaining: Some(0),
                retry_after: Some(Duration::from_secs(30)),
                ..ObservedQuota::default()
            },
            now,
        );
        let entries = store.snapshot_at(now);
        assert_eq!(entries.len(), 1, "one account per upstream has one entry");
        assert_eq!(entries[0].status_class, RATE_LIMITED);
        assert_eq!(entries[0].retry_after_seconds, Some(30));
        assert_eq!(entries[0].quota.requests_remaining, Some(0));
        assert_eq!(entries[0].mode, "observed");
    }

    #[test]
    fn cooldown_counts_down_and_accounts_are_kept_apart() {
        let store = ObservedStore::new();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        store.record(
            "oauth",
            "alpha",
            RATE_LIMITED,
            Some("rate_limited"),
            ObservedQuota {
                retry_after: Some(Duration::from_secs(30)),
                ..ObservedQuota::default()
            },
            now,
        );
        store.record("oauth", "beta", OK, None, ObservedQuota::default(), now);
        assert_eq!(store.snapshot_at(now).len(), 2);
        let cooling = store.cooldowns(now);
        assert_eq!(cooling.len(), 1);
        assert_eq!(cooling[0].account, "alpha");
        assert_eq!(cooling[0].seconds_remaining, 30);
        let cooling = store.cooldowns(now + Duration::from_secs(10));
        assert_eq!(cooling[0].seconds_remaining, 20);
        assert!(store.cooldowns(now + Duration::from_secs(31)).is_empty());
    }
}
