//! Pure data layer for a terminal view of peer key health: parse a `/v1/health` document and
//! apply the `GET /v1/health/stream` events. No UI, no networking; the caller supplies every
//! clock reading.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub mod matrix;
#[cfg(feature = "tui")]
pub mod ui;
pub mod view;

/// The whole `/v1/health` document. Unknown fields are ignored; missing optional fields are
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// The published instance name, or the fixed `reiki`.
    pub instance: String,
    /// The publisher's version, cleaned by [`version_field`]; older publishers omit it.
    #[serde(default, deserialize_with = "version_field")]
    pub reiki_version: Option<String>,
    /// The publisher's UTC clock when the answer was built; for people and logs.
    pub observed_at: String,
    /// How old the figures were when the peer sent them; older publishers omit it.
    #[serde(default)]
    pub age_seconds: Option<u64>,
    pub upstreams: Vec<Upstream>,
    /// Counts of recent key failures by class over the peer's bounded ring. Counts only: the
    /// bodies behind them never leave the peer.
    #[serde(default)]
    pub failures: BTreeMap<String, u64>,
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
pub struct Failure {
    /// Unix seconds.
    pub time: u64,
    pub stream: String,
    pub key: String,
    #[serde(default)]
    pub status: Option<u16>,
    pub class: String,
    #[serde(default)]
    pub retry_after: Option<String>,
    #[serde(default)]
    pub cooled: bool,
    #[serde(default)]
    pub cooling_seconds: Option<u64>,
    #[serde(default)]
    pub note: String,
}

/// Printable ASCII, at most 32 characters, so a peer's version cannot spoof a terminal line.
fn version_field<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::String(text)) => {
            let clean: String = text
                .chars()
                .filter(|c| (' '..='~').contains(c))
                .take(32)
                .collect();
            (!clean.is_empty()).then_some(clean)
        }
        _ => None,
    })
}

/// One upstream entry: counts by key state, never key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    pub name: String,
    pub healthy_keys: u32,
    pub cooling_keys: u32,
    pub disabled_keys: u32,
    /// How many (route, model) combinations are cooling on this upstream's keys. A count only:
    /// the names behind it are never published. Older publishers omit it.
    #[serde(default)]
    pub cooling_combinations: u32,
    /// The shortest wait among the cooling keys, or `null` when none is cooling.
    #[serde(default)]
    pub soonest_cooldown_seconds: Option<u64>,
    /// `2xx`, `4xx` or `5xx` for the latest attempt, or `null` before one.
    #[serde(default)]
    pub last_status_class: Option<String>,
    /// Labelled keys, only when the peer sets `expose_key_labels`.
    #[serde(default)]
    pub keys: Option<Vec<KeyView>>,
    /// Attempts in the last hour; absent from an older publisher or with `expose_usage = false`.
    #[serde(default, deserialize_with = "usage_u64")]
    pub requests_1h: Option<u64>,
    /// Attempts that were rate-limited or began a cooldown in the last hour.
    #[serde(default, deserialize_with = "usage_u32")]
    pub limited_1h: Option<u32>,
    /// Twelve five-minute attempt counts, oldest first; extra buckets are dropped.
    #[serde(default, deserialize_with = "usage_buckets")]
    pub request_buckets: Option<[u32; 12]>,
    /// Input plus output tokens in the last hour; absent from an older publisher.
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

/// One labelled key. `cooldown_class` is set only for a cooling key whose class is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyView {
    pub name: String,
    /// `available`, `cooling` or `out_of_rotation`.
    pub state: String,
    #[serde(default)]
    pub cooldown_seconds: Option<u64>,
    #[serde(default)]
    pub cooldown_class: Option<String>,
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

    /// Patches one upstream of one peer from a `key` event, whose upstream entry is complete.
    fn apply_key_event(&mut self, name: &str, data: &str) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Event {
            upstream: Option<EventUpstream>,
        }
        #[derive(Deserialize)]
        struct EventUpstream {
            name: String,
            #[serde(default)]
            healthy_keys: Option<u32>,
            #[serde(default)]
            cooling_keys: Option<u32>,
            #[serde(default)]
            disabled_keys: Option<u32>,
            #[serde(default)]
            cooling_combinations: Option<u32>,
            #[serde(default)]
            soonest_cooldown_seconds: Option<u64>,
            #[serde(default)]
            last_status_class: Option<String>,
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
        let event: Event = serde_json::from_str(data).map_err(|e| e.to_string())?;
        let Some(event) = event.upstream else {
            return Err("key event has no upstream".to_owned());
        };
        let Some(peer) = self.peers.get_mut(name) else {
            return Ok(());
        };
        let Some(upstream) = peer
            .health
            .upstreams
            .iter_mut()
            .find(|u| u.name == event.name)
        else {
            return Ok(());
        };
        if let Some(n) = event.healthy_keys {
            upstream.healthy_keys = n;
        }
        if let Some(n) = event.cooling_keys {
            upstream.cooling_keys = n;
        }
        if let Some(n) = event.disabled_keys {
            upstream.disabled_keys = n;
        }
        if let Some(n) = event.cooling_combinations {
            upstream.cooling_combinations = n;
        }
        if let Some(n) = event.soonest_cooldown_seconds {
            upstream.soonest_cooldown_seconds = Some(n);
        }
        if let Some(class) = event.last_status_class {
            upstream.last_status_class = Some(class);
        }
        // The event carries the whole upstream entry, so its key list replaces ours and a
        // removed key does not linger until the next snapshot.
        if let Some(incoming) = event.keys {
            upstream.keys = Some(incoming);
        }
        if let Some(n) = event.requests_1h {
            upstream.requests_1h = Some(n);
        }
        if let Some(n) = event.limited_1h {
            upstream.limited_1h = Some(n);
        }
        if let Some(buckets) = event.request_buckets {
            upstream.request_buckets = Some(buckets);
        }
        if let Some(n) = event.tokens_1h {
            upstream.tokens_1h = Some(n);
        }
        if let Some(buckets) = event.token_buckets {
            upstream.token_buckets = Some(buckets);
        }
        if let Some(minutes) = event.request_minutes {
            upstream.request_minutes = Some(minutes);
        }
        if let Some(minutes) = event.token_minutes {
            upstream.token_minutes = Some(minutes);
        }
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
        "instance": "peer-a",
        "observed_at": "2026-11-13T09:30:00Z",
        "age_seconds": 0,
        "upstreams": [
            {"name": "zen", "healthy_keys": 1, "cooling_keys": 1, "disabled_keys": 0,
             "cooling_combinations": 2,
             "soonest_cooldown_seconds": 42, "last_status_class": "4xx"}
        ],
        "host": {"load_average": [0.52, 0.58, 0.59], "cores": 8,
                 "memory": {"total_bytes": 16384000000, "available_bytes": 8192000000,
                            "swap_total_bytes": 2048000000, "swap_free_bytes": 2048000000},
                 "disks": [{"roles": ["config", "state"], "total_bytes": 107374182400,
                            "available_bytes": 53687091200, "used_percent": 50.0}]},
        "sessions": {"window_seconds": 300, "active_sessions": 3}
    }"#;

    /// The same document from a publisher with host stats and labels off.
    const BARE_DOC: &str = r#"{"instance":"peer-a","observed_at":"2026-11-13T09:30:00Z","upstreams":[
        {"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
         "soonest_cooldown_seconds":42,"last_status_class":"4xx"}]}"#;

    /// A document carrying the last hour of attempts.
    const USAGE_DOC: &str = r#"{"instance":"peer-a","observed_at":"2026-11-13T09:30:00Z",
        "upstreams":[{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
        "soonest_cooldown_seconds":null,"last_status_class":"2xx",
        "requests_1h":120,"limited_1h":3,
        "request_buckets":[1,2,3,4,5,6,7,8,9,10,11,12]}]}"#;

    /// A labelled view whose cooling key carries the quota class.
    const LABELLED_DOC: &str = r#"{"instance":"peer-a","observed_at":"2026-11-13T09:30:00Z",
        "upstreams":[{"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
        "soonest_cooldown_seconds":300,"last_status_class":"4xx",
        "keys":[{"name":"a","state":"available","cooldown_seconds":null},
                {"name":"b","state":"cooling","cooldown_seconds":300,
                 "cooldown_class":"quota_exhausted"},
                {"name":"c","state":"out_of_rotation","cooldown_seconds":null}]}]}"#;

    fn fleet() -> Fleet {
        Fleet::new(Duration::from_secs(90))
    }

    #[test]
    fn failure_counts_parse_and_default_to_empty() {
        let with = r#"{"instance":"peer-a","observed_at":"t","upstreams":[],
            "failures":{"rate_limited":2,"transient":1}}"#;
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
        assert_eq!(health.age_seconds, Some(0));
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
    fn usage_fields_parse_and_an_older_document_has_none() {
        let health = Health::from_json(USAGE_DOC).expect("a usage document parses");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.requests_1h, Some(120));
        assert_eq!(upstream.limited_1h, Some(3));
        assert_eq!(
            upstream.request_buckets,
            Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
        );
        let old = Health::from_json(BARE_DOC).expect("an old document parses");
        assert_eq!(old.upstreams[0].requests_1h, None);
        assert_eq!(old.upstreams[0].limited_1h, None);
        assert_eq!(old.upstreams[0].request_buckets, None);
    }

    #[test]
    fn hostile_usage_numbers_are_clamped_not_rejected() {
        let health = Health::from_json(
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "requests_1h":18446744073709551615,
                 "limited_1h":18446744073709551615,
                 "request_buckets":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]}]}"#,
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
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "requests_1h":-5,"limited_1h":1.9,
                 "request_buckets":[-1,1.9,"x",3,4,5,6,7,8,9,10,11]}]}"#,
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
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "requests_1h":"many","limited_1h":{},"request_buckets":"not an array"}]}"#,
        )
        .expect("wrong types do not reject the document");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.requests_1h, None);
        assert_eq!(upstream.limited_1h, None);
        assert_eq!(upstream.request_buckets, None);
    }

    #[test]
    fn token_fields_parse_clamp_and_stay_absent_from_older_documents() {
        let health = Health::from_json(
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "tokens_1h":18446744073709551615,
                 "token_buckets":[1,2,3,4,5,6,7,8,9,10,11,12,13]}]}"#,
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
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "tokens_1h":-5,"token_buckets":[-1,1.9,"x",3,4,5,6,7,8,9,10,11]}]}"#,
        )
        .expect("negative, float and string numbers parse");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.tokens_1h, Some(0));
        assert_eq!(
            upstream.token_buckets,
            Some([0, 1, 0, 3, 4, 5, 6, 7, 8, 9, 10, 11])
        );

        let health = Health::from_json(
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "tokens_1h":"many","token_buckets":"not an array"}]}"#,
        )
        .expect("wrong types do not reject the document");
        assert_eq!(health.upstreams[0].tokens_1h, None);
        assert_eq!(health.upstreams[0].token_buckets, None);

        let old = Health::from_json(BARE_DOC).expect("an old document parses");
        assert_eq!(old.upstreams[0].tokens_1h, None);
        assert_eq!(old.upstreams[0].token_buckets, None);
    }

    #[test]
    fn minute_fields_parse_clamp_and_stay_absent_from_older_documents() {
        let health = Health::from_json(
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "request_minutes":[1,2,3,4,5,6,7,8,9,10,11,12],
                 "token_minutes":[1,"x",-1,1.9,4,5,6,7,8,18446744073709551615]}]}"#,
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
            r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                 "request_minutes":"many","token_minutes":{"a":1}}]}"#,
        )
        .expect("wrong types do not reject the document");
        assert_eq!(health.upstreams[0].request_minutes, None);
        assert_eq!(health.upstreams[0].token_minutes, None);

        let old = Health::from_json(BARE_DOC).expect("an old document parses");
        assert_eq!(old.upstreams[0].request_minutes, None);
        assert_eq!(old.upstreams[0].token_minutes, None);
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
                r#"{"upstream":{"name":"zen","healthy_keys":1,"cooling_keys":0,
                    "disabled_keys":0,"requests_1h":7,"limited_1h":1,
                    "request_buckets":[0,0,0,0,0,0,0,0,0,0,1,6],
                    "tokens_1h":99,"token_buckets":[0,0,0,0,0,0,0,0,0,0,9,90],
                    "request_minutes":[0,0,0,0,0,0,0,0,0,4],
                    "token_minutes":[0,0,0,0,0,0,0,0,0,90]}}"#,
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
    fn a_document_without_cooling_combinations_parses_as_zero() {
        let health = Health::from_json(BARE_DOC).expect("an old document parses");
        assert_eq!(health.upstreams[0].cooling_combinations, 0);
    }

    #[test]
    fn a_document_without_host_and_sessions_parses() {
        let health = Health::from_json(BARE_DOC).expect("a bare document parses");
        assert!(health.host.is_none());
        assert!(health.sessions.is_none());
        assert!(health.age_seconds.is_none(), "age_seconds is optional");
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
        assert_eq!(keys[2].state, "out_of_rotation");
    }

    #[test]
    fn unknown_fields_are_ignored_and_nulls_stay_none() {
        let health = Health::from_json(
            r#"{"instance":"peer-a","observed_at":"2026-11-13T09:30:00Z","future_field":42,
                "upstreams":[{"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":0,
                "soonest_cooldown_seconds":null,"last_status_class":null,"extra":true}]}"#,
        )
        .expect("unknown fields are ignored");
        let upstream = &health.upstreams[0];
        assert_eq!(upstream.soonest_cooldown_seconds, None);
        assert_eq!(upstream.last_status_class, None);
        assert_eq!(upstream.keys, None);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        for text in ["", "{", "not json", r#"{"instance":1,"upstreams":[]}"#] {
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
                r#"{"upstream":{"name":"gone","healthy_keys":1,"cooling_keys":0,
                    "disabled_keys":0,"last_status_class":"2xx"}}"#,
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
                r#"{"upstream":{"name":"zen","healthy_keys":1}}"#,
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
            .expect_err("a key event without an upstream errors");
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
                r#"{"instance":"peer-a","observed_at":"2026-11-13T09:31:00Z",
                    "upstreams":[{"name":"spare","healthy_keys":0,"cooling_keys":0,
                    "disabled_keys":1,"soonest_cooldown_seconds":null,
                    "last_status_class":"5xx"}]}"#,
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
                r#"{"instance":"peer-a","observed_at":"t","upstreams":[
                    {"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
                     "soonest_cooldown_seconds":42,"last_status_class":"4xx"},
                    {"name":"open","healthy_keys":2,"cooling_keys":0,"disabled_keys":0,
                     "soonest_cooldown_seconds":null,"last_status_class":null}]}"#,
            )
            .unwrap(),
            now,
        );
        fleet.apply_snapshot(
            "peer-b",
            Health::from_json(
                r#"{"instance":"peer-b","observed_at":"t","upstreams":[
                    {"name":"zen","healthy_keys":0,"cooling_keys":2,"disabled_keys":1,
                     "soonest_cooldown_seconds":300,"last_status_class":"4xx"},
                    {"name":"gem","healthy_keys":3,"cooling_keys":0,"disabled_keys":0,
                     "soonest_cooldown_seconds":null,"last_status_class":"2xx"}]}"#,
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
        let doc = r#"{"instance":"peer-a","observed_at":"t","upstreams":[
            {"name":"zen","healthy_keys":0,"cooling_keys":1,"disabled_keys":0,
             "soonest_cooldown_seconds":90,
             "keys":[{"name":"k","state":"cooling","cooldown_seconds":90}]}]}"#;
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
    fn the_version_is_read_when_present_and_none_when_an_older_peer_omits_it() {
        let with =
            r#"{"instance":"peer-a","observed_at":"t","reiki_version":"0.10.3","upstreams":[]}"#;
        assert_eq!(
            Health::from_json(with).unwrap().reiki_version.as_deref(),
            Some("0.10.3")
        );
        assert_eq!(
            Health::from_json(BARE_DOC).unwrap().reiki_version,
            None,
            "an older peer"
        );
    }

    #[test]
    fn a_hostile_version_keeps_printable_ascii_and_at_most_32_chars() {
        let doc = |value: &str| {
            format!(
                r#"{{"instance":"p","observed_at":"t","reiki_version":{value},"upstreams":[]}}"#
            )
        };
        let long = format!("\"{}\"", "9".repeat(40));
        let cases = [
            (
                r#""0.10.3\u0007\u202e""#.to_owned(),
                Some("0.10.3".to_owned()),
            ),
            (r#""héllo-1""#.to_owned(), Some("hllo-1".to_owned())),
            (long, Some("9".repeat(32))),
            (r#""\u0001\n""#.to_owned(), None),
            (r#""""#.to_owned(), None),
            ("42".to_owned(), None),
            ("null".to_owned(), None),
            (r#"["0.10.3"]"#.to_owned(), None),
        ];
        for (value, expected) in cases {
            let health = Health::from_json(&doc(&value))
                .unwrap_or_else(|e| panic!("{value} is not an error: {e}"));
            assert_eq!(health.reiki_version, expected, "{value}");
        }
    }
}
