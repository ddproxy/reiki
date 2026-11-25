//! Shared wire types for Reiki peer health documents and `reiki top` events.
//!
//! Both the publisher and the reader use these types, so the JSON shape is defined in one place.
//! Readers ignore unknown fields and unknown enum-like values for forward compatibility; only the
//! config-gated fields may be absent.

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// The whole `/v1/health` document in the current wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// The published instance name, or the fixed `reiki`.
    pub instance: String,
    /// The publisher's version, cleaned by [`version_field`].
    #[serde(deserialize_with = "version_field")]
    pub reiki_version: String,
    /// The publisher's UTC clock when the answer was built; for people and logs.
    pub observed_at: String,
    /// How old the figures were when the peer sent them.
    pub age_seconds: u64,
    pub upstreams: Vec<Upstream>,
    /// Passthrough upstreams Reiki has observed. These carry no key state; a reader synthesises
    /// display-only upstream entries so they can be seen beside keyed upstreams.
    pub observed: Vec<ObservedUpstream>,
    /// Counts of recent key failures by class over the peer's bounded ring. Counts only: the
    /// bodies behind them never leave the peer.
    pub failures: BTreeMap<String, u64>,
    /// Total request-summary exports dropped by the peer's bounded queue or failed syslog sends.
    #[serde(default)]
    pub requestlog_export_dropped_total: u64,
    /// Only present when the peer sets `expose_host_stats`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<Value>,
    /// Only present when the peer sets `expose_host_stats`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Value>,
    /// Aggregate gateway counters, absent with `expose_usage = false`. Counts only: never stream
    /// names, model names or anything derived from a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayCounts>,
    /// Stream counts, absent with `expose_usage = false` or on peers running an older version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streams: Option<StreamCounts>,
}

/// Aggregate stream counts published to peers. Names are omitted: a dynamic stream name can
/// identify a tenant, while a count says only how many streams a peer serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCounts {
    pub static_streams: u64,
    pub dynamic_streams: u64,
}

/// Aggregate gateway counters published to peers. Every field is a count with a fixed name, so a
/// peer sees how much traffic a gateway admitted without learning who sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayCounts {
    /// Requests admitted across all streams.
    pub requests: u64,
    /// Tokens charged across all streams, measured or estimated.
    pub tokens: u64,
    /// Counter-store failures observed by the gateway.
    pub store_errors: u64,
}

impl Health {
    /// Parses a `/v1/health` document or a `snapshot` event body. Malformed input is an error,
    /// never a panic.
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

/// One recent key failure, as `/v1/failures` returns it. Only the local instance's admin
/// listener supplies these; a remote peer publishes counts only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// Unix seconds.
    pub time: u64,
    pub stream: String,
    pub key: String,
    pub status: Option<u16>,
    pub class: String,
    /// What the failover walk did after this failure; older documents read as pass-through.
    #[serde(default = "default_failure_action")]
    pub action: String,
    pub retry_after: Option<String>,
    pub cooled: bool,
    pub cooling_seconds: Option<u64>,
    /// Scrubbed body snippet; `null` when the caller holds only the read-only admin token.
    pub note: Option<String>,
}

fn default_failure_action() -> String {
    "pass-through".to_owned()
}

/// Quota numbers a passthrough upstream reported in its response headers. Every field is present
/// on the wire and may be `null`; other JSON types are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedQuota {
    #[serde(deserialize_with = "required_nullable_u64")]
    pub requests_limit: Option<u64>,
    #[serde(deserialize_with = "required_nullable_u64")]
    pub requests_remaining: Option<u64>,
    #[serde(deserialize_with = "required_nullable_u64")]
    pub requests_reset_seconds: Option<u64>,
    #[serde(deserialize_with = "required_nullable_u64")]
    pub tokens_limit: Option<u64>,
    #[serde(deserialize_with = "required_nullable_u64")]
    pub tokens_remaining: Option<u64>,
    #[serde(deserialize_with = "required_nullable_u64")]
    pub tokens_reset_seconds: Option<u64>,
}

/// One observed passthrough account. `retry_after_seconds` is the remaining wait on the host's
/// clock, not a value copied from the upstream; `null` means it has elapsed or was never reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedUpstream {
    #[serde(deserialize_with = "safe_label_field")]
    pub upstream: String,
    #[serde(deserialize_with = "safe_label_field")]
    pub account: String,
    #[serde(deserialize_with = "safe_label_field")]
    pub mode: String,
    #[serde(deserialize_with = "safe_label_field")]
    pub status_class: String,
    #[serde(deserialize_with = "safe_label_field")]
    pub observed_at: String,
    #[serde(deserialize_with = "required_nullable_u64")]
    pub retry_after_seconds: Option<u64>,
    #[serde(deserialize_with = "required_nullable_label")]
    pub cooldown_class: Option<String>,
    pub quota: ObservedQuota,
}

fn required_nullable_u64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .map(Some)
        .ok_or_else(|| D::Error::custom("expected an unsigned integer or null"))
}

fn required_nullable_label<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    let Value::String(text) = value else {
        return Err(D::Error::custom("expected a string or null"));
    };
    if let Some(reason) = safe_label_problem(&text) {
        return Err(D::Error::custom(format!("cooldown_class {reason}")));
    }
    Ok(Some(text))
}

fn safe_label_field<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let Value::String(text) = Value::deserialize(deserializer)? else {
        return Err(D::Error::custom("expected a string"));
    };
    if let Some(reason) = safe_label_problem(&text) {
        return Err(D::Error::custom(reason));
    }
    Ok(text)
}

/// Printable ASCII, at most 32 characters, so a peer's version cannot spoof a terminal line.
/// A missing, null or non-string version is an error: the current wire shape always has one.
fn version_field<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let Value::String(text) = Value::deserialize(deserializer)? else {
        return Err(D::Error::custom("reiki_version must be a string"));
    };
    let clean: String = text
        .chars()
        .filter(|c| (' '..='~').contains(c))
        .take(32)
        .collect();
    if clean.is_empty() {
        return Err(D::Error::custom(
            "reiki_version has no printable characters",
        ));
    }
    Ok(clean)
}

/// A terminal-safe operator label: short printable ASCII, never blank. Values outside this set
/// are refused on both config load and the wire, so no peer can inject control sequences.
pub fn safe_label_problem(text: &str) -> Option<&'static str> {
    if text.is_empty() || text.len() > 64 {
        return Some("must be 1 to 64 characters long");
    }
    if text.bytes().any(|b| !(b' '..=b'~').contains(&b)) {
        return Some("must contain only printable ASCII characters");
    }
    if text.trim().is_empty() {
        return Some("must not be blank");
    }
    None
}

fn safe_label_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let Some(Value::String(text)) = Option::<Value>::deserialize(deserializer)? else {
        return Err(D::Error::custom("capacity_group must be a string"));
    };
    if let Some(reason) = safe_label_problem(&text) {
        return Err(D::Error::custom(format!("capacity_group {reason}")));
    }
    Ok(Some(text))
}

/// An `Option` that must actually be present in JSON; `null` remains a valid value. Serde treats
/// a missing field of type `Option<T>` as `None`, so nullable required fields need this wrapper on
/// the wire type.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequiredNullable<T>(Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for RequiredNullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Going through `Option::deserialize` would also succeed on serde's missing-field
        // deserializer, so an absent key would look like `null`. Reading a `Value` first keeps
        // absence an error while still accepting an explicit JSON null.
        let value = Value::deserialize(deserializer)?;
        let inner = if value.is_null() {
            None
        } else {
            Some(T::deserialize(value).map_err(D::Error::custom)?)
        };
        Ok(Self(inner))
    }
}

/// One upstream entry: counts by key state, never key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "UpstreamWire")]
pub struct Upstream {
    pub name: String,
    pub healthy_keys: u32,
    pub cooling_keys: u32,
    pub disabled_keys: u32,
    /// How many (route, model) combinations are cooling on this upstream's keys. A count only:
    /// the names behind it are never published.
    pub cooling_combinations: u32,
    /// The shortest wait among the cooling keys, or `null` when none is cooling.
    pub soonest_cooldown_seconds: Option<u64>,
    /// `2xx`, `4xx` or `5xx` for the latest attempt, or `null` before one.
    pub last_status_class: Option<String>,
    /// Operator-set opaque group for this upstream, absent until configured. Never derived from
    /// the upstream name, a key label or a response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_group: Option<String>,
    /// Labelled keys, only when the peer sets `expose_key_labels`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<KeyView>>,
    /// Attempts in the last hour; absent with `expose_usage = false`.
    #[serde(
        default,
        deserialize_with = "usage_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub requests_1h: Option<u64>,
    /// Attempts that were rate-limited or began a cooldown in the last hour.
    #[serde(
        default,
        deserialize_with = "usage_u32",
        skip_serializing_if = "Option::is_none"
    )]
    pub limited_1h: Option<u32>,
    /// Twelve five-minute attempt counts, oldest first; extra buckets are dropped.
    #[serde(
        default,
        deserialize_with = "usage_buckets",
        skip_serializing_if = "Option::is_none"
    )]
    pub request_buckets: Option<[u32; 12]>,
    /// Input plus output tokens in the last hour; absent with `expose_usage = false`.
    #[serde(
        default,
        deserialize_with = "usage_u64",
        skip_serializing_if = "Option::is_none"
    )]
    pub tokens_1h: Option<u64>,
    /// Twelve five-minute token counts, oldest first; extra buckets are dropped.
    #[serde(
        default,
        deserialize_with = "token_buckets",
        skip_serializing_if = "Option::is_none"
    )]
    pub token_buckets: Option<[u64; 12]>,
    /// Ten one-minute attempt counts, oldest first and newest last; extra minutes are dropped.
    #[serde(
        default,
        deserialize_with = "request_minutes",
        skip_serializing_if = "Option::is_none"
    )]
    pub request_minutes: Option<[u32; 10]>,
    /// Ten one-minute token counts, oldest first and newest last; extra minutes are dropped.
    #[serde(
        default,
        deserialize_with = "token_minutes",
        skip_serializing_if = "Option::is_none"
    )]
    pub token_minutes: Option<[u32; 10]>,
}

/// The wire form of [`Upstream`]. The required nullable pair is enforced here; the public
/// struct keeps plain `Option` fields for callers. Unknown fields are ignored for forward
/// compatibility.
#[derive(Deserialize)]
struct UpstreamWire {
    name: String,
    healthy_keys: u32,
    cooling_keys: u32,
    disabled_keys: u32,
    cooling_combinations: u32,
    soonest_cooldown_seconds: RequiredNullable<u64>,
    last_status_class: RequiredNullable<String>,
    #[serde(default, deserialize_with = "safe_label_option")]
    capacity_group: Option<String>,
    #[serde(default)]
    keys: Option<Vec<KeyView>>,
    #[serde(default, deserialize_with = "usage_u64")]
    requests_1h: Option<u64>,
    #[serde(default, deserialize_with = "usage_u32")]
    limited_1h: Option<u32>,
    #[serde(default, deserialize_with = "usage_buckets")]
    request_buckets: Option<[u32; 12]>,
    #[serde(default, deserialize_with = "usage_u64")]
    tokens_1h: Option<u64>,
    #[serde(default, deserialize_with = "token_buckets")]
    token_buckets: Option<[u64; 12]>,
    #[serde(default, deserialize_with = "request_minutes")]
    request_minutes: Option<[u32; 10]>,
    #[serde(default, deserialize_with = "token_minutes")]
    token_minutes: Option<[u32; 10]>,
}

impl From<UpstreamWire> for Upstream {
    fn from(wire: UpstreamWire) -> Self {
        Self {
            name: wire.name,
            healthy_keys: wire.healthy_keys,
            cooling_keys: wire.cooling_keys,
            disabled_keys: wire.disabled_keys,
            cooling_combinations: wire.cooling_combinations,
            soonest_cooldown_seconds: wire.soonest_cooldown_seconds.0,
            last_status_class: wire.last_status_class.0,
            capacity_group: wire.capacity_group,
            keys: wire.keys,
            requests_1h: wire.requests_1h,
            limited_1h: wire.limited_1h,
            request_buckets: wire.request_buckets,
            tokens_1h: wire.tokens_1h,
            token_buckets: wire.token_buckets,
            request_minutes: wire.request_minutes,
            token_minutes: wire.token_minutes,
        }
    }
}

/// A JSON number as a clamped count: other types read as absent, never as a parse error.
fn usage_number(value: &Value) -> Option<u64> {
    let Value::Number(number) = value else {
        return None;
    };
    if let Some(n) = number.as_u64() {
        return Some(n);
    }
    if let Some(n) = number.as_i64() {
        return Some(u64::try_from(n).unwrap_or(0));
    }
    let f = number.as_f64()?;
    if !f.is_finite() || f <= 0.0 {
        Some(0)
    } else if f >= u64::MAX as f64 {
        Some(u64::MAX)
    } else {
        Some(f as u64)
    }
}

fn usage_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Ok(Option::<Value>::deserialize(deserializer)?
        .as_ref()
        .and_then(usage_number))
}

fn usage_u32<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    Ok(usage_u64(deserializer)?.map(|n| u32::try_from(n).unwrap_or(u32::MAX)))
}

/// The newest `N` entries of a count array, each clamped; other shapes read as absent.
fn counts<'de, D: Deserializer<'de>, const N: usize>(
    deserializer: D,
) -> Result<Option<[u64; N]>, D::Error> {
    let Some(Value::Array(items)) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let mut out = [0u64; N];
    let start = items.len().saturating_sub(N);
    for (slot, item) in out.iter_mut().zip(&items[start..]) {
        *slot = usage_number(item).unwrap_or(0);
    }
    Ok(Some(out))
}

/// `u64` counts narrowed to `u32`, saturating.
fn narrow<const N: usize>(counts: [u64; N]) -> [u32; N] {
    counts.map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn usage_buckets<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<[u32; 12]>, D::Error> {
    Ok(counts::<D, 12>(deserializer)?.map(narrow))
}

fn token_buckets<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<[u64; 12]>, D::Error> {
    counts::<D, 12>(deserializer)
}

fn request_minutes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<[u32; 10]>, D::Error> {
    Ok(counts::<D, 10>(deserializer)?.map(narrow))
}

fn token_minutes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<[u32; 10]>, D::Error> {
    Ok(counts::<D, 10>(deserializer)?.map(narrow))
}

/// One labelled key. `cooldown_class` is set only for a cooling key whose class is known, and
/// `quota_reset_at` only for a quota cooldown with a known reset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "KeyViewWire")]
pub struct KeyView {
    pub name: String,
    /// `available`, `cooling` or `out_of_rotation`.
    pub state: String,
    pub cooldown_seconds: Option<u64>,
    /// Only published for a cooling key whose class is known, so absence is valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_class: Option<String>,
    /// RFC3339 UTC reset, only for a cooling quota key with a reported reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_reset_at: Option<String>,
}

#[derive(Deserialize)]
struct KeyViewWire {
    name: String,
    state: String,
    cooldown_seconds: RequiredNullable<u64>,
    #[serde(default)]
    cooldown_class: Option<String>,
    #[serde(default)]
    quota_reset_at: Option<String>,
}

impl From<KeyViewWire> for KeyView {
    fn from(wire: KeyViewWire) -> Self {
        Self {
            name: wire.name,
            state: wire.state,
            cooldown_seconds: wire.cooldown_seconds.0,
            cooldown_class: wire.cooldown_class,
            quota_reset_at: wire.quota_reset_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{
        "instance": "peer-a",
        "reiki_version": "0.12.0",
        "observed_at": "2026-11-22T00:00:00Z",
        "age_seconds": 0,
        "upstreams": [{
            "name": "alpha",
            "healthy_keys": 1,
            "cooling_keys": 0,
            "disabled_keys": 0,
            "cooling_combinations": 0,
            "soonest_cooldown_seconds": null,
            "last_status_class": null,
            "keys": [{"name": "k", "state": "available", "cooldown_seconds": null}]
        }],
        "observed": [],
        "failures": {"rate_limited": 2},
        "requestlog_export_dropped_total": 3,
        "host": {"cores": 4},
        "sessions": {"active_sessions": 1}
    }"#;

    #[test]
    fn the_shared_type_round_trips_the_existing_wire_shape() {
        let expected: Value = serde_json::from_str(BASE).unwrap();
        let health = Health::from_json(BASE).unwrap();
        assert_eq!(health.upstreams[0].capacity_group, None);
        assert_eq!(serde_json::to_value(health).unwrap(), expected);
    }

    #[test]
    fn an_older_failure_without_an_action_reads_as_pass_through() {
        let old = r#"{
            "time": 1,
            "stream": "s",
            "key": "k",
            "status": 429,
            "class": "rate_limited",
            "retry_after": "30",
            "cooled": false,
            "cooling_seconds": null,
            "note": null
        }"#;
        let failure: Failure = serde_json::from_str(old).unwrap();
        assert_eq!(failure.action, "pass-through");
        let current = Failure {
            action: "rotate".to_owned(),
            ..failure
        };
        let text = serde_json::to_string(&current).unwrap();
        assert!(text.contains("\"action\":\"rotate\""), "{text}");
    }

    #[test]
    fn a_capacity_group_round_trips_and_is_never_inferred() {
        let text = BASE.replace(
            "\"last_status_class\": null,",
            "\"last_status_class\": null, \"capacity_group\": \"team-a\",",
        );
        let health = Health::from_json(&text).unwrap();
        assert_eq!(
            health.upstreams[0].capacity_group.as_deref(),
            Some("team-a")
        );
        let value = serde_json::to_value(health).unwrap();
        assert_eq!(value["upstreams"][0]["capacity_group"], "team-a");
        // The key label is not a capacity group, and no such field appears inside a key view.
        assert_eq!(value["upstreams"][0]["keys"][0]["name"], "k");
        assert!(value["upstreams"][0]["keys"][0]
            .get("capacity_group")
            .is_none());
    }

    #[test]
    fn gateway_counts_round_trip_and_stay_absent_by_default() {
        let bare = Health::from_json(BASE).unwrap();
        assert_eq!(bare.gateway, None);
        let text = BASE.replace(
            "\"sessions\": {\"active_sessions\": 1}",
            "\"sessions\": {\"active_sessions\": 1}, \"gateway\": \
             {\"requests\": 12, \"tokens\": 345, \"store_errors\": 1}",
        );
        let health = Health::from_json(&text).unwrap();
        assert_eq!(
            health.gateway,
            Some(GatewayCounts {
                requests: 12,
                tokens: 345,
                store_errors: 1,
            })
        );
        let value = serde_json::to_value(health).unwrap();
        assert_eq!(value["gateway"]["requests"], 12);
        assert_eq!(value["gateway"]["tokens"], 345);
        assert_eq!(value["gateway"]["store_errors"], 1);
        let unknown = text.replace("\"requests\": 12,", "\"requests\": 12, \"surprise\": 1,");
        let health = Health::from_json(&unknown).unwrap();
        assert_eq!(health.gateway.unwrap().requests, 12);
    }

    #[test]
    fn an_unsafe_capacity_group_is_refused() {
        for group in ["", "   ", "a\u{1b}[2J", "x\ny", &"x".repeat(65)] {
            let text = BASE.replace(
                "\"last_status_class\": null,",
                &format!(
                    "\"last_status_class\": null, \"capacity_group\": {},",
                    serde_json::to_string(group).unwrap()
                ),
            );
            assert!(
                Health::from_json(&text).is_err(),
                "{group:?} must not be accepted"
            );
        }
    }

    #[test]
    fn observed_entries_round_trip_with_required_nullable_fields() {
        let text = BASE.replace(
            "\"observed\": [],",
            r#""observed": [{
                "upstream": "oauth",
                "account": "alpha",
                "mode": "observed",
                "status_class": "rate_limited",
                "observed_at": "2026-11-22T00:00:00Z",
                "retry_after_seconds": 30,
                "cooldown_class": "rate_limited",
                "quota": {
                    "requests_limit": 60,
                    "requests_remaining": 0,
                    "requests_reset_seconds": null,
                    "tokens_limit": null,
                    "tokens_remaining": null,
                    "tokens_reset_seconds": null
                }
            }],"#,
        );
        let health = Health::from_json(&text).unwrap();
        assert_eq!(health.observed.len(), 1);
        assert_eq!(health.observed[0].retry_after_seconds, Some(30));
        assert_eq!(health.observed[0].quota.requests_remaining, Some(0));
        assert_eq!(
            serde_json::to_value(health).unwrap()["observed"][0]["mode"],
            "observed"
        );

        let missing = text.replace("\"retry_after_seconds\": 30,", "");
        assert!(
            Health::from_json(&missing).is_err(),
            "a missing key is an error"
        );
        let wrong_type = text.replace(
            "\"retry_after_seconds\": 30,",
            "\"retry_after_seconds\": \"30\",",
        );
        assert!(Health::from_json(&wrong_type).is_err());
        let unknown = text.replace(
            "\"mode\": \"observed\",",
            "\"mode\": \"observed\", \"surprise\": 1,",
        );
        let health = Health::from_json(&unknown).unwrap();
        assert_eq!(health.observed[0].mode, "observed");
    }

    #[test]
    fn unknown_fields_and_enum_like_values_are_ignored_for_forward_compat() {
        let text = BASE.replace(
            "\"last_status_class\": null,",
            "\"last_status_class\": \"7xx\", \"surprise\": 1, \"capacity_group\": \"team-a\",",
        );
        let text = text.replace(
            "\"cooldown_seconds\": null}]",
            "\"cooldown_seconds\": null, \"cooldown_class\": \"future\", \"quota_reset_at\": null, \"extra_key\": true}]",
        );
        let text = text.replace(
            "\"sessions\": {\"active_sessions\": 1}",
            "\"sessions\": {\"active_sessions\": 1, \"extra_session\": 1}, \"gateway\": {\"requests\":1,\"tokens\":2,\"store_errors\":3,\"extra\":4}, \"streams\": {\"static_streams\":1,\"dynamic_streams\":2,\"extra\":3}, \"extra_top\": {\"x\":1}",
        );
        let health = Health::from_json(&text).unwrap();
        assert_eq!(
            health.upstreams[0].last_status_class.as_deref(),
            Some("7xx")
        );
        assert_eq!(
            health.upstreams[0].keys.as_ref().unwrap()[0]
                .cooldown_class
                .as_deref(),
            Some("future")
        );
        assert_eq!(health.gateway.unwrap().requests, 1);
        assert_eq!(health.streams.unwrap().static_streams, 1);
    }
}
