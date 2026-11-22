//! Pure data layer for a terminal view of peer key health: parse a `/v1/health` document and
//! apply the `GET /v1/health/stream` events. No UI, no networking; the caller supplies every
//! clock reading.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub mod matrix;
#[cfg(feature = "tui")]
pub mod ui;
pub mod view;

/// The whole `/v1/health` document in the current wire shape. Unknown fields are an error; only
/// the config-gated fields may be absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Counts of recent key failures by class over the peer's bounded ring. Counts only: the
    /// bodies behind them never leave the peer.
    pub failures: BTreeMap<String, u64>,
    /// Total request-summary exports dropped by the peer's bounded queue or failed syslog sends.
    #[serde(default)]
    pub requestlog_export_dropped_total: u64,
    /// Only present when the peer sets `expose_host_stats`.
    #[serde(default)]
    pub host: Option<Value>,
    /// Only present when the peer sets `expose_host_stats`.
    #[serde(default)]
    pub sessions: Option<Value>,
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
#[serde(deny_unknown_fields)]
pub struct Failure {
    /// Unix seconds.
    pub time: u64,
    pub stream: String,
    pub key: String,
    pub status: Option<u16>,
    pub class: String,
    pub retry_after: Option<String>,
    pub cooled: bool,
    pub cooling_seconds: Option<u64>,
    pub note: String,
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
    /// Labelled keys, only when the peer sets `expose_key_labels`.
    #[serde(default)]
    pub keys: Option<Vec<KeyView>>,
    /// Attempts in the last hour; absent with `expose_usage = false`.
    #[serde(default, deserialize_with = "usage_u64")]
    pub requests_1h: Option<u64>,
    /// Attempts that were rate-limited or began a cooldown in the last hour.
    #[serde(default, deserialize_with = "usage_u32")]
    pub limited_1h: Option<u32>,
    /// Twelve five-minute attempt counts, oldest first; extra buckets are dropped.
    #[serde(default, deserialize_with = "usage_buckets")]
    pub request_buckets: Option<[u32; 12]>,
    /// Input plus output tokens in the last hour; absent with `expose_usage = false`.
    #[serde(default, deserialize_with = "usage_u64")]
    pub tokens_1h: Option<u64>,
    /// Twelve five-minute token counts, oldest first; extra buckets are dropped.
    #[serde(default, deserialize_with = "token_buckets")]
    pub token_buckets: Option<[u64; 12]>,
    /// Ten one-minute attempt counts, oldest first and newest last; extra minutes are dropped.
    #[serde(default, deserialize_with = "request_minutes")]
    pub request_minutes: Option<[u32; 10]>,
    /// Ten one-minute token counts, oldest first and newest last; extra minutes are dropped.
    #[serde(default, deserialize_with = "token_minutes")]
    pub token_minutes: Option<[u32; 10]>,
}

/// The wire form of [`Upstream`]. `deny_unknown_fields` and the required nullable pair are
/// enforced here; the public struct keeps plain `Option` fields for callers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamWire {
    name: String,
    healthy_keys: u32,
    cooling_keys: u32,
    disabled_keys: u32,
    cooling_combinations: u32,
    soonest_cooldown_seconds: RequiredNullable<u64>,
    last_status_class: RequiredNullable<String>,
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
    #[serde(default)]
    pub cooldown_class: Option<String>,
    /// RFC3339 UTC reset, only for a cooling quota key with a reported reset.
    #[serde(default)]
    pub quota_reset_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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

/// One peer's health plus when this host received it.
#[derive(Debug, Clone)]
struct Peer {
    health: Health,
    received_at: Instant,
    /// Cooldowns are counted down to this instant by [`Fleet::advance`].
    advanced_at: Instant,
}

/// Health from every peer (the label the caller gives each peer), for a TUI to render.
#[derive(Debug, Clone)]
pub struct Fleet {
    /// Data older than this is stale, and stale never reads healthy.
    max_age: Duration,
    peers: HashMap<String, Peer>,
    /// Why a peer's last feed failed, shown until a snapshot arrives.
    errors: HashMap<String, String>,
    /// Full recent failures for the local instance's published name, when its admin listener
    /// was reachable. Keyed by `Health::instance`, never by the local peer label.
    local_failures: HashMap<String, Vec<Failure>>,
}

/// One upstream row of the provider summary: counts summed over every fresh peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub upstream: String,
    pub healthy_keys: u32,
    pub total_keys: u32,
    pub peer_count: u32,
}

/// A borrowed view of one peer's health for the view layer: the data plus its age on this host's
/// clock. `stale` matches the rule `provider_summary` uses: exactly `max_age` old is fresh.
#[derive(Debug)]
pub struct PeerView<'a> {
    pub health: &'a Health,
    /// How long ago this host received the health.
    pub age: Duration,
    pub stale: bool,
}

impl Fleet {
    pub fn new(max_age: Duration) -> Self {
        Self {
            max_age,
            peers: HashMap::new(),
            errors: HashMap::new(),
            local_failures: HashMap::new(),
        }
    }

    /// Replaces the full failure list for one published instance.
    pub fn set_local_failures(&mut self, instance: &str, failures: Vec<Failure>) {
        self.local_failures.insert(instance.to_owned(), failures);
    }

    /// The full failure list for one published instance, or `None` when this host did not
    /// fetch it (a remote peer, or no admin token).
    pub fn local_failures(&self, instance: &str) -> Option<&[Failure]> {
        self.local_failures.get(instance).map(Vec::as_slice)
    }

    /// Replaces a peer's health, received at `now`, and clears its last feed error.
    pub fn apply_snapshot(&mut self, name: &str, health: Health, now: Instant) {
        self.errors.remove(name);
        self.peers.insert(
            name.to_owned(),
            Peer {
                health,
                received_at: now,
                advanced_at: now,
            },
        );
    }

    /// Records why a peer's feed failed. The reason stays until the next snapshot, so a peer that
    /// is down is never mistaken for one that is merely quiet.
    pub fn set_error(&mut self, name: &str, error: impl Into<String>) {
        self.errors.insert(name.to_owned(), error.into());
    }

    /// The peer's last feed error, if any.
    pub fn error(&self, name: &str) -> Option<&str> {
        self.errors.get(name).map(String::as_str)
    }

    /// Applies one SSE event body. `snapshot` replaces the peer's health; `key` patches one
    /// upstream; every other event is ignored. A patch for an unknown peer or upstream is
    /// ignored, not an error.
    pub fn apply_event(
        &mut self,
        name: &str,
        event_name: &str,
        data: &str,
        now: Instant,
    ) -> Result<(), String> {
        match event_name {
            "snapshot" => {
                let health = Health::from_json(data)?;
                self.apply_snapshot(name, health, now);
                Ok(())
            }
            "key" => {
                self.apply_key_event(name, data)?;
                self.touch(name, now);
                Ok(())
            }
            _ => {
                self.touch(name, now);
                Ok(())
            }
        }
    }

    /// Counts every cooldown down by the time since the last call. A peer republishes only when a
    /// key changes, so without this a cooldown would sit at its published value until then.
    pub fn advance(&mut self, now: Instant) {
        for peer in self.peers.values_mut() {
            let secs = now.saturating_duration_since(peer.advanced_at).as_secs();
            if secs == 0 {
                continue;
            }
            // Only whole seconds are consumed, so the remainder carries to the next call.
            peer.advanced_at += Duration::from_secs(secs);
            for upstream in &mut peer.health.upstreams {
                if let Some(left) = upstream.soonest_cooldown_seconds.as_mut() {
                    *left = left.saturating_sub(secs);
                }
                for key in upstream.keys.iter_mut().flatten() {
                    if let Some(left) = key.cooldown_seconds.as_mut() {
                        *left = left.saturating_sub(secs);
                    }
                }
            }
        }
    }

    /// Marks a peer's data as received at `now`. A live stream proves the data is current even
    /// when nothing changed, so callers also call this for each `: heartbeat` comment.
    pub fn touch(&mut self, name: &str, now: Instant) {
        if let Some(peer) = self.peers.get_mut(name) {
            peer.received_at = now;
        }
    }

    /// Replaces one upstream of one peer from a `key` event. The event carries the whole upstream
    /// entry, so a removed key does not linger until the next snapshot.
    fn apply_key_event(&mut self, name: &str, data: &str) -> Result<(), String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Event {
            /// Accepted for shape validation; freshness comes from when this host received it.
            #[allow(dead_code)]
            observed_at: String,
            #[allow(dead_code)]
            age_seconds: u64,
            upstream: Upstream,
        }
        let event: Event = serde_json::from_str(data).map_err(|e| e.to_string())?;
        let Some(peer) = self.peers.get_mut(name) else {
            return Ok(());
        };
        let Some(upstream) = peer
            .health
            .upstreams
            .iter_mut()
            .find(|u| u.name == event.upstream.name)
        else {
            return Ok(());
        };
        *upstream = event.upstream;
        Ok(())
    }

    /// The peer's health view, or `None` when this host has nothing for it.
    pub fn peer(&self, name: &str, now: Instant) -> Option<PeerView<'_>> {
        self.peers.get(name).map(|peer| {
            let age = now.saturating_duration_since(peer.received_at);
            PeerView {
                health: &peer.health,
                age,
                stale: age > self.max_age,
            }
        })
    }

    /// One row per upstream name, summed over every fresh peer, in upstream-name order.
    pub fn provider_summary(&self, now: Instant) -> Vec<ProviderRow> {
        let mut rows: HashMap<&str, ProviderRow> = HashMap::new();
        for (name, peer) in &self.peers {
            if self.stale(name, now, self.max_age) {
                continue;
            }
            for upstream in &peer.health.upstreams {
                let row = rows
                    .entry(upstream.name.as_str())
                    .or_insert_with(|| ProviderRow {
                        upstream: upstream.name.clone(),
                        healthy_keys: 0,
                        total_keys: 0,
                        peer_count: 0,
                    });
                row.healthy_keys = row.healthy_keys.saturating_add(upstream.healthy_keys);
                row.total_keys = row.total_keys.saturating_add(
                    upstream
                        .healthy_keys
                        .saturating_add(upstream.cooling_keys)
                        .saturating_add(upstream.disabled_keys),
                );
                row.peer_count += 1;
            }
        }
        let mut out: Vec<ProviderRow> = rows.into_values().collect();
        out.sort_by(|a, b| a.upstream.cmp(&b.upstream));
        out
    }

    /// True when the peer has no data, or its data is older than `max_age`.
    pub fn stale(&self, name: &str, now: Instant, max_age: Duration) -> bool {
        match self.peers.get(name) {
            None => true,
            Some(peer) => now.saturating_duration_since(peer.received_at) > max_age,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn events_and_heartbeats_keep_a_quiet_ct_fresh() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(Duration::from_secs(30));
        fleet.apply_snapshot("peer-a", Health::from_json(BARE_DOC).unwrap(), t0);
        let later = t0 + Duration::from_secs(40);
        assert!(fleet.stale("peer-a", later, Duration::from_secs(30)));
        fleet.touch("peer-a", later);
        assert!(!fleet.stale("peer-a", later, Duration::from_secs(30)));
        let much_later = later + Duration::from_secs(40);
        fleet
            .apply_event("peer-a", "host", "{}", much_later)
            .unwrap();
        assert!(!fleet.stale("peer-a", much_later, Duration::from_secs(30)));
        fleet.touch("unknown", much_later);
    }

    use super::*;

    /// A full document, with host stats and sessions, as a publisher with everything on sends.
    const FULL_DOC: &str = r#"{
   "age_seconds" : 0,
   "failures" : {},
   "host" : {
      "cores" : 8,
      "disks" : [
         {
            "available_bytes" : 53687091200,
            "roles" : [
               "config",
               "state"
            ],
            "total_bytes" : 107374182400,
            "used_percent" : 50
         }
      ],
      "load_average" : [
         0.52,
         0.58,
         0.59
      ],
      "memory" : {
         "available_bytes" : 8192000000,
         "swap_free_bytes" : 2048000000,
         "swap_total_bytes" : 2048000000,
         "total_bytes" : 16384000000
      }
   },
   "instance" : "peer-a",
   "observed_at" : "2026-11-13T09:30:00Z",
   "reiki_version" : "0.10.3",
   "sessions" : {
      "active_sessions" : 3,
      "window_seconds" : 300
   },
   "upstreams" : [
      {
         "cooling_combinations" : 2,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : "4xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : 42
      }
   ]
}
"#;

    /// The same document from a publisher with host stats and labels off.
    const BARE_DOC: &str = r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "2026-11-13T09:30:00Z",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : "4xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : 42
      }
   ]
}
"#;

    /// A document carrying the last hour of attempts.
    const USAGE_DOC: &str = r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "2026-11-13T09:30:00Z",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : "2xx",
         "limited_1h" : 3,
         "name" : "zen",
         "request_buckets" : [
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            12
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 120,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 0
      }
   ]
}
"#;

    /// A labelled view whose cooling key carries the quota class.
    const LABELLED_DOC: &str = r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "2026-11-13T09:30:00Z",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "keys" : [
            {
               "cooldown_seconds" : null,
               "name" : "a",
               "state" : "available"
            },
            {
               "cooldown_class" : "quota_exhausted",
               "cooldown_seconds" : 300,
               "name" : "b",
               "quota_reset_at" : "2026-11-14T00:00:00Z",
               "state" : "cooling"
            },
            {
               "cooldown_seconds" : null,
               "name" : "c",
               "state" : "out_of_rotation"
            }
         ],
         "last_status_class" : "4xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : 300
      }
   ]
}
"#;

    fn fleet() -> Fleet {
        Fleet::new(Duration::from_secs(90))
    }

    #[test]
    fn failure_counts_parse_and_default_to_empty() {
        let with = r#"{
   "age_seconds" : 0,
   "failures" : {
      "rate_limited" : 2,
      "transient" : 1
   },
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : []
}
"#;
        let health = Health::from_json(with).unwrap();
        assert_eq!(health.failures.get("rate_limited"), Some(&2));
        assert_eq!(health.failures.get("transient"), Some(&1));
        let bare = Health::from_json(BARE_DOC).unwrap();
        assert!(bare.failures.is_empty());
    }

    #[test]
    fn local_failures_are_keyed_by_published_instance() {
        let mut fleet = fleet();
        fleet.set_local_failures(
            "peer-a",
            vec![Failure {
                time: 1,
                stream: "s".to_owned(),
                key: "k".to_owned(),
                status: Some(429),
                class: "rate_limited".to_owned(),
                retry_after: Some("120".to_owned()),
                cooled: true,
                cooling_seconds: Some(1),
                note: "n".to_owned(),
            }],
        );
        assert_eq!(fleet.local_failures("peer-a").unwrap().len(), 1);
        assert!(fleet.local_failures("peer-b").is_none());
    }

    #[test]
    fn a_full_document_parses_with_host_and_sessions() {
        let health = Health::from_json(FULL_DOC).expect("a full document parses");
        assert_eq!(health.instance, "peer-a");
        assert_eq!(health.observed_at, "2026-11-13T09:30:00Z");
        assert_eq!(health.age_seconds, 0);
        assert_eq!(health.upstreams.len(), 1);
        assert_eq!(
            health.upstreams[0],
            Upstream {
                name: "zen".to_owned(),
                healthy_keys: 1,
                cooling_keys: 1,
                disabled_keys: 0,
                cooling_combinations: 2,
                soonest_cooldown_seconds: Some(42),
                last_status_class: Some("4xx".to_owned()),
                keys: None,
                requests_1h: None,
                limited_1h: None,
                request_buckets: None,
                tokens_1h: None,
                token_buckets: None,
                request_minutes: None,
                token_minutes: None,
            }
        );
        assert!(health.host.is_some(), "host stats are kept");
        assert!(health.sessions.is_some(), "session counts are kept");
    }

    #[test]
    fn usage_fields_parse_absent_figures_stay_none() {
        let health = Health::from_json(USAGE_DOC).expect("a usage document parses");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.requests_1h, Some(120));
        assert_eq!(upstream.limited_1h, Some(3));
        assert_eq!(
            upstream.request_buckets,
            Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
        );
        let bare = Health::from_json(BARE_DOC).expect("a bare document parses");
        assert_eq!(bare.upstreams[0].requests_1h, None);
        assert_eq!(bare.upstreams[0].limited_1h, None);
        assert_eq!(bare.upstreams[0].request_buckets, None);
    }

    #[test]
    fn hostile_usage_numbers_are_clamped_not_rejected() {
        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 18446744073709551615,
         "name" : "zen",
         "request_buckets" : [
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            12,
            13,
            14,
            15
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 18446744073709551615,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 0
      }
   ]
}
"#,
        )
        .expect("large numbers parse");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.requests_1h, Some(u64::MAX));
        assert_eq!(upstream.limited_1h, Some(u32::MAX));
        assert_eq!(
            upstream.request_buckets,
            Some([4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
            "only the newest twelve buckets are kept"
        );

        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 1.9,
         "name" : "zen",
         "request_buckets" : [
            -1,
            1.9,
            "x",
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : -5,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 0
      }
   ]
}
"#,
        )
        .expect("negative, float and string numbers parse");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.requests_1h, Some(0), "negative clamps to zero");
        assert_eq!(upstream.limited_1h, Some(1), "a float truncates");
        assert_eq!(
            upstream.request_buckets,
            Some([0, 1, 0, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
            "bad bucket entries read as zero"
        );

        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : {},
         "name" : "zen",
         "request_buckets" : "not an array",
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : "many",
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 0
      }
   ]
}
"#,
        )
        .expect("wrong types do not reject the document");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.requests_1h, None);
        assert_eq!(upstream.limited_1h, None);
        assert_eq!(upstream.request_buckets, None);
    }

    #[test]
    fn token_fields_parse_clamp_and_stay_absent_without_exposure() {
        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 0,
         "name" : "zen",
         "request_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 0,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            12,
            13
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : 18446744073709551615
      }
   ]
}
"#,
        )
        .expect("a document with tokens parses");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.tokens_1h, Some(u64::MAX));
        assert_eq!(
            upstream.token_buckets,
            Some([2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
            "only the newest twelve buckets are kept"
        );

        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 0,
         "name" : "zen",
         "request_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 0,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            -1,
            1.9,
            "x",
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11
         ],
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : -5
      }
   ]
}
"#,
        )
        .expect("negative, float and string numbers parse");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.tokens_1h, Some(0));
        assert_eq!(
            upstream.token_buckets,
            Some([0, 1, 0, 3, 4, 5, 6, 7, 8, 9, 10, 11])
        );

        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 0,
         "name" : "zen",
         "request_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "request_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "requests_1h" : 0,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : "not an array",
         "token_minutes" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "tokens_1h" : "many"
      }
   ]
}
"#,
        )
        .expect("wrong types do not reject the document");
        assert_eq!(health.upstreams[0].tokens_1h, None);
        assert_eq!(health.upstreams[0].token_buckets, None);

        let bare = Health::from_json(BARE_DOC).expect("a bare document parses");
        assert_eq!(bare.upstreams[0].tokens_1h, None);
        assert_eq!(bare.upstreams[0].token_buckets, None);
    }

    #[test]
    fn minute_fields_parse_clamp_and_stay_absent_without_exposure() {
        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 0,
         "name" : "zen",
         "request_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "request_minutes" : [
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            12
         ],
         "requests_1h" : 0,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : [
            1,
            "x",
            -1,
            1.9,
            4,
            5,
            6,
            7,
            8,
            18446744073709551615
         ],
         "tokens_1h" : 0
      }
   ]
}
"#,
        )
        .expect("minute arrays parse");
        let upstream = &health.upstreams[0];
        assert_eq!(
            upstream.request_minutes,
            Some([3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
            "only the newest ten minutes are kept"
        );
        assert_eq!(
            upstream.token_minutes,
            Some([1, 0, 0, 1, 4, 5, 6, 7, 8, u32::MAX]),
            "hostile values clamp, a huge one saturates at u32"
        );

        let health = Health::from_json(
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "limited_1h" : 0,
         "name" : "zen",
         "request_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "request_minutes" : "many",
         "requests_1h" : 0,
         "soonest_cooldown_seconds" : null,
         "token_buckets" : [
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0
         ],
         "token_minutes" : {
            "a" : 1
         },
         "tokens_1h" : 0
      }
   ]
}
"#,
        )
        .expect("wrong types do not reject the document");
        assert_eq!(health.upstreams[0].request_minutes, None);
        assert_eq!(health.upstreams[0].token_minutes, None);

        let bare = Health::from_json(BARE_DOC).expect("a bare document parses");
        assert_eq!(bare.upstreams[0].request_minutes, None);
        assert_eq!(bare.upstreams[0].token_minutes, None);
    }

    #[test]
    fn a_key_event_carries_usage_fields() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot("peer-a", Health::from_json(USAGE_DOC).unwrap(), now);
        fleet
            .apply_event(
                "peer-a",
                "key",
                r#"{
   "age_seconds" : 0,
   "observed_at" : "2026-11-13T09:30:05Z",
   "upstream" : {
      "cooling_combinations" : 0,
      "cooling_keys" : 0,
      "disabled_keys" : 0,
      "healthy_keys" : 1,
      "last_status_class" : null,
      "limited_1h" : 1,
      "name" : "zen",
      "request_buckets" : [
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         1,
         6
      ],
      "request_minutes" : [
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         4
      ],
      "requests_1h" : 7,
      "soonest_cooldown_seconds" : null,
      "token_buckets" : [
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         9,
         90
      ],
      "token_minutes" : [
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         0,
         90
      ],
      "tokens_1h" : 99
   }
}
"#,
                now,
            )
            .expect("a key event applies");
        let upstream = &fleet.peers["peer-a"].health.upstreams[0];
        assert_eq!(upstream.requests_1h, Some(7));
        assert_eq!(upstream.limited_1h, Some(1));
        assert_eq!(
            upstream.request_buckets,
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 6])
        );
        assert_eq!(upstream.tokens_1h, Some(99));
        assert_eq!(
            upstream.token_buckets,
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 90])
        );
        assert_eq!(
            upstream.request_minutes,
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 4])
        );
        assert_eq!(
            upstream.token_minutes,
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 90])
        );
    }

    #[test]
    fn a_document_without_host_and_sessions_parses() {
        let health = Health::from_json(BARE_DOC).expect("a bare document parses");
        assert!(health.host.is_none());
        assert!(health.sessions.is_none());
        assert_eq!(health.age_seconds, 0);
        assert!(health.failures.is_empty());
    }

    #[test]
    fn nullable_fields_accept_explicit_null() {
        let health = Health::from_json(
            r#"{"instance":"peer-a","reiki_version":"0.10.3","observed_at":"t",
                "age_seconds":0,"failures":{},"upstreams":[
                {"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":0,
                 "cooling_combinations":0,"soonest_cooldown_seconds":null,
                 "last_status_class":null}]}"#,
        )
        .expect("null is a value, not an absent field");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.soonest_cooldown_seconds, None);
        assert_eq!(upstream.last_status_class, None);
        assert_eq!(upstream.keys, None);
    }

    #[test]
    fn a_labelled_view_shows_the_cooldown_class_of_a_quota_key() {
        let health = Health::from_json(LABELLED_DOC).expect("a labelled document parses");
        let keys = health.upstreams[0].keys.as_ref().expect("keys are kept");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].state, "available");
        assert_eq!(keys[0].cooldown_class, None, "no class while available");
        assert_eq!(keys[1].state, "cooling");
        assert_eq!(keys[1].cooldown_seconds, Some(300));
        assert_eq!(keys[1].cooldown_class.as_deref(), Some("quota_exhausted"));
        assert_eq!(
            keys[1].quota_reset_at.as_deref(),
            Some("2026-11-14T00:00:00Z")
        );
        assert_eq!(keys[0].quota_reset_at, None, "available keys have no reset");
        assert_eq!(keys[2].state, "out_of_rotation");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let docs = [
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","age_seconds":0,
                 "failures":{},"upstreams":[],"future_field":42}"#,
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","age_seconds":0,
                 "failures":{},"upstreams":[{"name":"zen","healthy_keys":0,"cooling_keys":0,
                 "disabled_keys":0,"cooling_combinations":0,"soonest_cooldown_seconds":null,
                 "last_status_class":null,"extra":true}]}"#,
        ];
        for doc in docs {
            assert!(Health::from_json(doc).is_err(), "{doc}");
        }
    }

    #[test]
    fn required_fields_are_enforced() {
        let docs = [
            // Missing top-level figures.
            r#"{"instance":"p","observed_at":"t","age_seconds":0,"failures":{},"upstreams":[]}"#,
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","failures":{},"upstreams":[]}"#,
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","age_seconds":0,"upstreams":[]}"#,
            // Missing upstream figures, including the required nullable pair.
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","age_seconds":0,"failures":{},
                 "upstreams":[{"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":0,
                 "soonest_cooldown_seconds":null,"last_status_class":null}]}"#,
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","age_seconds":0,"failures":{},
                 "upstreams":[{"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":0,
                 "cooling_combinations":0,"last_status_class":null}]}"#,
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","age_seconds":0,"failures":{},
                 "upstreams":[{"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":0,
                 "cooling_combinations":0,"soonest_cooldown_seconds":null}]}"#,
            // A labelled key must carry its nullable cooldown too.
            r#"{"instance":"p","reiki_version":"0","observed_at":"t","age_seconds":0,"failures":{},
                 "upstreams":[{"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
                 "cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":null,
                 "keys":[{"name":"k","state":"cooling"}]}]}"#,
        ];
        for doc in docs {
            assert!(Health::from_json(doc).is_err(), "{doc}");
        }
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        for text in [
            "",
            "{",
            "not json",
            r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : 1,
   "reiki_version" : "0.10.3",
   "upstreams" : []
}
"#,
        ] {
            let err = Health::from_json(text).expect_err("malformed input errors");
            assert!(!err.is_empty());
        }
    }

    #[test]
    fn a_key_event_flips_one_key_to_cooling() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot("peer-a", Health::from_json(LABELLED_DOC).unwrap(), now);
        fleet
            .apply_event(
                "peer-a",
                "key",
                r#"{"observed_at":"2026-11-13T09:30:05Z","age_seconds":0,
                    "upstream":{"name":"zen","healthy_keys":0,"cooling_keys":2,"disabled_keys":0,
                    "cooling_combinations":2,
                    "soonest_cooldown_seconds":300,"last_status_class":"4xx",
                    "keys":[{"name":"a","state":"cooling","cooldown_seconds":300,
                             "cooldown_class":"rate_limited"},
                            {"name":"b","state":"cooling","cooldown_seconds":300,
                             "cooldown_class":"quota_exhausted"}]}}"#,
                now,
            )
            .expect("a key event applies");
        let upstream = &fleet.peers["peer-a"].health.upstreams[0];
        assert_eq!(upstream.healthy_keys, 0);
        assert_eq!(upstream.cooling_keys, 2);
        assert_eq!(upstream.cooling_combinations, 2);
        let a = &upstream.keys.as_ref().unwrap()[0];
        assert_eq!(a.state, "cooling");
        assert_eq!(a.cooldown_class.as_deref(), Some("rate_limited"));
        // The event is the whole upstream entry, so a key it omits is gone.
        assert_eq!(upstream.keys.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn a_key_event_for_an_unknown_upstream_is_ignored() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot("peer-a", Health::from_json(BARE_DOC).unwrap(), now);
        fleet
            .apply_event(
                "peer-a",
                "key",
                r#"{
   "age_seconds" : 0,
   "observed_at" : "2026-11-13T09:30:05Z",
   "upstream" : {
      "cooling_combinations" : 0,
      "cooling_keys" : 0,
      "disabled_keys" : 0,
      "healthy_keys" : 1,
      "last_status_class" : "2xx",
      "name" : "gone",
      "soonest_cooldown_seconds" : null
   }
}
"#,
                now,
            )
            .expect("an unknown upstream is not an error");
        assert_eq!(fleet.peers["peer-a"].health.upstreams.len(), 1);
        assert_eq!(fleet.peers["peer-a"].health.upstreams[0].name, "zen");
    }

    #[test]
    fn a_key_event_for_an_unknown_ct_is_ignored() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet
            .apply_event(
                "peer-d",
                "key",
                r#"{"observed_at":"t","age_seconds":0,"upstream":{
                    "name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                    "cooling_combinations":0,"soonest_cooldown_seconds":null,
                    "last_status_class":null}}"#,
                now,
            )
            .expect("an unknown peer is not an error");
        assert!(fleet.peers.is_empty());
    }

    #[test]
    fn a_key_event_with_bad_data_is_an_error() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot("peer-a", Health::from_json(BARE_DOC).unwrap(), now);
        let err = fleet
            .apply_event("peer-a", "key", "{not json", now)
            .expect_err("malformed data errors");
        assert!(!err.is_empty());
        let err = fleet
            .apply_event("peer-a", "key", r#"{"observed_at":"x"}"#, now)
            .expect_err("a partial key event errors");
        assert!(!err.is_empty());
    }

    #[test]
    fn a_snapshot_event_replaces_everything() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot("peer-a", Health::from_json(LABELLED_DOC).unwrap(), now);
        fleet
            .apply_event(
                "peer-a",
                "snapshot",
                r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "2026-11-13T09:31:00Z",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 1,
         "healthy_keys" : 0,
         "last_status_class" : "5xx",
         "name" : "spare",
         "soonest_cooldown_seconds" : null
      }
   ]
}
"#,
                now + Duration::from_secs(1),
            )
            .expect("a snapshot event applies");
        let health = &fleet.peers["peer-a"].health;
        assert_eq!(health.upstreams.len(), 1, "the old upstream is gone");
        assert_eq!(health.upstreams[0].name, "spare");
        assert_eq!(health.upstreams[0].disabled_keys, 1);
        assert!(health.upstreams[0].keys.is_none());
    }

    #[test]
    fn a_snapshot_event_with_malformed_data_errors() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet
            .apply_event("peer-a", "snapshot", "garbage", now)
            .expect_err("a malformed snapshot errors");
        assert!(fleet.peers.is_empty(), "nothing is stored");
    }

    #[test]
    fn other_events_are_ignored() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot("peer-a", Health::from_json(BARE_DOC).unwrap(), now);
        for (name, data) in [
            ("host", r#"{"host":{"cores":8}}"#),
            ("sessions", r#"{"sessions":{"active_sessions":2}}"#),
            ("unknown", r#"{"x":1}"#),
        ] {
            fleet
                .apply_event("peer-a", name, data, now)
                .expect("ignored");
        }
        assert_eq!(fleet.peers["peer-a"].health.upstreams[0].healthy_keys, 1);
    }

    #[test]
    fn provider_summary_adds_two_cts() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot(
            "peer-a",
            Health::from_json(
                r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : "4xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : 42
      },
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 2,
         "last_status_class" : null,
         "name" : "open",
         "soonest_cooldown_seconds" : null
      }
   ]
}
"#,
            )
            .unwrap(),
            now,
        );
        fleet.apply_snapshot(
            "peer-b",
            Health::from_json(
                r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-b",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 2,
         "disabled_keys" : 1,
         "healthy_keys" : 0,
         "last_status_class" : "4xx",
         "name" : "zen",
         "soonest_cooldown_seconds" : 300
      },
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 3,
         "last_status_class" : "2xx",
         "name" : "gem",
         "soonest_cooldown_seconds" : null
      }
   ]
}
"#,
            )
            .unwrap(),
            now,
        );
        let rows = fleet.provider_summary(now);
        let zen = rows.iter().find(|r| r.upstream == "zen").expect("zen row");
        assert_eq!(zen.healthy_keys, 1);
        assert_eq!(zen.total_keys, 5, "2 keys from peer-a and 3 from peer-b");
        assert_eq!(zen.peer_count, 2);
        let open = rows
            .iter()
            .find(|r| r.upstream == "open")
            .expect("open row");
        assert_eq!(
            (open.healthy_keys, open.total_keys, open.peer_count),
            (2, 2, 1)
        );
        let gem = rows.iter().find(|r| r.upstream == "gem").expect("gem row");
        assert_eq!(
            (gem.healthy_keys, gem.total_keys, gem.peer_count),
            (3, 3, 1)
        );
        assert_eq!(rows.len(), 3, "one row per upstream, sorted by name");
        assert_eq!(
            rows.iter().map(|r| r.upstream.as_str()).collect::<Vec<_>>(),
            ["gem", "open", "zen"]
        );
    }

    #[test]
    fn a_feed_error_stays_until_a_snapshot_arrives() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.set_error("peer-a", "stream: the stream ended; poll: cannot connect");
        assert_eq!(
            fleet.error("peer-a"),
            Some("stream: the stream ended; poll: cannot connect")
        );
        assert_eq!(fleet.error("peer-b"), None, "errors are per peer");
        fleet.apply_snapshot("peer-a", Health::from_json(BARE_DOC).unwrap(), now);
        assert_eq!(fleet.error("peer-a"), None, "a snapshot clears the error");
    }

    #[test]
    fn stale_cts_are_left_out_of_the_summary() {
        let now = Instant::now();
        let old = now - Duration::from_secs(40);
        let mut fleet = Fleet::new(Duration::from_secs(30));
        fleet.apply_snapshot("peer-a", Health::from_json(BARE_DOC).unwrap(), now); // fresh
        fleet.apply_snapshot("peer-b", Health::from_json(BARE_DOC).unwrap(), old); // 40s old -> stale
        assert!(!fleet.stale("peer-a", now, Duration::from_secs(30)));
        assert!(fleet.stale("peer-b", now, Duration::from_secs(30)));
        let rows = fleet.provider_summary(now);
        assert_eq!(rows.len(), 1, "only the fresh peer counts");
        assert_eq!(rows[0].peer_count, 1);
    }

    #[test]
    fn unknown_and_missing_cts_are_stale() {
        let now = Instant::now();
        let fleet = fleet();
        assert!(fleet.stale("nobody", now, Duration::from_secs(30)));
    }

    #[test]
    fn data_exactly_max_age_old_is_fresh_and_older_is_stale() {
        let now = Instant::now();
        let mut fleet = fleet();
        fleet.apply_snapshot("peer-a", Health::from_json(BARE_DOC).unwrap(), now);
        assert!(!fleet.stale(
            "peer-a",
            now + Duration::from_secs(90),
            Duration::from_secs(90)
        ));
        assert!(fleet.stale(
            "peer-a",
            now + Duration::from_secs(91),
            Duration::from_secs(90)
        ));
    }

    #[test]
    fn cooldowns_count_down_between_snapshots_and_never_below_zero() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(Duration::from_secs(30));
        let doc = r#"{
   "age_seconds" : 0,
   "failures" : {},
   "instance" : "peer-a",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 0,
         "keys" : [
            {
               "cooldown_seconds" : 90,
               "name" : "k",
               "state" : "cooling"
            }
         ],
         "last_status_class" : null,
         "name" : "zen",
         "soonest_cooldown_seconds" : 90
      }
   ]
}
"#;
        fleet.apply_snapshot("peer-a", Health::from_json(doc).unwrap(), t0);
        fleet.advance(t0 + Duration::from_millis(1500));
        let left = |f: &Fleet, at| f.peer("peer-a", at).unwrap().health.upstreams[0].clone();
        let at = t0 + Duration::from_millis(1500);
        assert_eq!(left(&fleet, at).soonest_cooldown_seconds, Some(89));
        assert_eq!(left(&fleet, at).keys.unwrap()[0].cooldown_seconds, Some(89));
        // The half second left over is not lost.
        fleet.advance(t0 + Duration::from_millis(2100));
        let at = t0 + Duration::from_millis(2100);
        assert_eq!(left(&fleet, at).soonest_cooldown_seconds, Some(88));
        fleet.advance(t0 + Duration::from_secs(10_000));
        let at = t0 + Duration::from_secs(10_000);
        assert_eq!(left(&fleet, at).soonest_cooldown_seconds, Some(0));
    }

    #[test]
    fn the_version_is_required_and_read() {
        assert_eq!(Health::from_json(BARE_DOC).unwrap().reiki_version, "0.10.3");
    }

    #[test]
    fn a_hostile_version_keeps_printable_ascii_and_at_most_32_chars() {
        let doc = |value: &str| {
            format!(
                r#"{{"instance":"p","reiki_version":{value},"observed_at":"t","age_seconds":0,
                    "failures":{{}},"upstreams":[]}}"#
            )
        };
        let long = format!("\"{}\"", "9".repeat(40));
        let cases = [
            (r#""0.10.3\u0007\u202e""#.to_owned(), "0.10.3".to_owned()),
            (r#""héllo-1""#.to_owned(), "hllo-1".to_owned()),
            (long, "9".repeat(32)),
        ];
        for (value, expected) in cases {
            let health = Health::from_json(&doc(&value))
                .unwrap_or_else(|e| panic!("{value} is not an error: {e}"));
            assert_eq!(health.reiki_version, expected, "{value}");
        }
        for value in [
            r#""\u0001\n""#.to_owned(),
            r#""""#.to_owned(),
            "42".to_owned(),
            "null".to_owned(),
            r#"["0.10.3"]"#.to_owned(),
        ] {
            Health::from_json(&doc(&value)).expect_err(&format!("{value} is not a version"));
        }
    }
}
