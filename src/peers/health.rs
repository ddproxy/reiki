//! The `/v1/health` document: per-upstream key counts, never key material, file paths, stream
//! names or request volume.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::chain::{KeyState, KeyStatus};
use crate::classify::FailureClass;
use crate::config::{Config, PeersConfig};
use crate::pressure::PressureCounter;
use crate::proxy::App;

/// Whole seconds, rounded up so a short wait never reads as zero, then up to a multiple of
/// `bucket` (exact for 0), so a reader learns only coarsely when a key comes back.
fn ceil_secs(wait: Duration, bucket: u64) -> u64 {
    let secs = (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1);
    if bucket == 0 {
        secs
    } else {
        secs.div_ceil(bucket).saturating_mul(bucket)
    }
}

fn cap<T>(mut items: Vec<T>, max: usize) -> (Vec<T>, bool) {
    let truncated = items.len() > max;
    items.truncate(max);
    (items, truncated)
}

fn status_class(status: Option<u16>) -> Option<String> {
    match status {
        Some(200..=299) => Some("2xx".to_owned()),
        Some(400..=499) => Some("4xx".to_owned()),
        Some(500..=599) => Some("5xx".to_owned()),
        _ => None,
    }
}

/// One entry per upstream that uses Reiki's keys, in config order.
pub(crate) fn upstreams(
    config: &Config,
    health: &mut KeyState,
    now: Instant,
    labels: bool,
    bucket: u64,
    pressure: Option<&PressureCounter>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, upstream) in &config.upstream {
        if upstream.passthrough {
            continue;
        }
        let keys: Vec<&str> = config
            .keys
            .iter()
            .filter(|(_, key)| key.upstream == *name)
            .map(|(key, _)| key.as_str())
            .collect();
        let (mut healthy, mut cooling, mut disabled) = (0u32, 0u32, 0u32);
        let mut combinations = 0u32;
        let mut soonest: Option<Duration> = None;
        let mut listed = Vec::new();
        for key in &keys {
            // A key cooling as a whole has no scoped entries left (cool_key clears them), so
            // this counts combination cooldowns only.
            combinations =
                combinations.saturating_add(health.cooling_scopes(key, now).len() as u32);
            let (state, wait) = match health.status(key, now) {
                KeyStatus::Available => {
                    healthy += 1;
                    ("available", None)
                }
                KeyStatus::Cooling(wait) => {
                    cooling += 1;
                    soonest = Some(soonest.map_or(wait, |s| s.min(wait)));
                    ("cooling", Some(ceil_secs(wait, bucket)))
                }
                KeyStatus::OutOfRotation => {
                    disabled += 1;
                    ("out_of_rotation", None)
                }
            };
            if labels {
                let mut cooldown_class = None;
                let mut quota_reset_at = None;
                if wait.is_some()
                    && let Some(class) = health.cooling_class(key, now)
                {
                    cooldown_class = Some(class.name().to_owned());
                    if class == FailureClass::QuotaExhausted {
                        quota_reset_at = health
                            .cooling_reset_at(key, now)
                            .map(crate::reload::rfc3339);
                    }
                }
                listed.push(reiki_health::KeyView {
                    name: (*key).to_owned(),
                    state: state.to_owned(),
                    cooldown_seconds: wait,
                    cooldown_class,
                    quota_reset_at,
                });
            }
        }
        let (
            requests_1h,
            limited_1h,
            request_buckets,
            tokens_1h,
            token_buckets,
            request_minutes,
            token_minutes,
        ) = if let Some(pressure) = pressure {
            let snapshot = pressure.snapshot(name, now);
            (
                Some(snapshot.requests_1h),
                Some(snapshot.limited_1h),
                Some(snapshot.buckets),
                Some(snapshot.tokens_1h),
                Some(snapshot.token_buckets),
                Some(snapshot.minutes),
                Some(snapshot.token_minutes),
            )
        } else {
            (None, None, None, None, None, None, None)
        };
        let (listed, keys_truncated) = cap(listed, MAX_KEYS_PER_UPSTREAM);
        let entry = reiki_health::Upstream {
            name: name.clone(),
            healthy_keys: healthy,
            cooling_keys: cooling,
            disabled_keys: disabled,
            cooling_combinations: combinations,
            soonest_cooldown_seconds: soonest.map(|wait| ceil_secs(wait, bucket)),
            last_status_class: health
                .latest_status(keys.iter().copied())
                .and_then(status_class),
            capacity_group: upstream.capacity_group.clone(),
            keys: labels.then_some(listed),
            keys_truncated,
            requests_1h,
            limited_1h,
            request_buckets,
            tokens_1h,
            token_buckets,
            request_minutes,
            token_minutes,
        };
        let value =
            serde_json::to_value(entry).expect("a health upstream entry always serializes to JSON");
        out.push(value);
    }
    cap(out, MAX_UPSTREAMS).0
}

/// Host figures with the disk array capped; the total roles for each disk are fixed by
/// [`disk_dirs`], so no further sub-array bound is needed.
pub(crate) async fn host_stats(app: &App, config: &Config) -> Value {
    let mut value = super::host::stats(&disk_dirs(app, config)).await;
    let mut truncated = false;
    if let Some(disks) = value.as_object_mut().and_then(|obj| obj.remove("disks"))
        && let Value::Array(items) = disks
    {
        let (items, over) = cap(items, MAX_DISKS);
        truncated = over;
        value["disks"] = json!(items);
    }
    value["disks_truncated"] = json!(truncated);
    value
}

/// The name this instance publishes: the configured one, or a fixed `reiki`, so a host,
/// container or node name never leaves the host unless the operator sets it.
pub(crate) fn instance_name(peers: &PeersConfig) -> String {
    peers
        .instance
        .clone()
        .unwrap_or_else(|| DEFAULT_INSTANCE.to_owned())
}

const DEFAULT_INSTANCE: &str = "reiki";

/// Published array limits. Counts and truncation flags keep a reader from treating a capped
/// document as complete; the limits are part of the provider health contract.
pub(crate) const MAX_UPSTREAMS: usize = 64;
pub(crate) const MAX_KEYS_PER_UPSTREAM: usize = 256;
pub(crate) const MAX_OBSERVED: usize = 256;
pub(crate) const MAX_DISKS: usize = 4;
pub(crate) const MAX_SESSION_STREAMS: usize = 256;

/// The whole `/v1/health` answer for the current snapshot and key health.
pub(crate) async fn document(app: &App, peers: &PeersConfig) -> Value {
    let snapshot = app.snapshot();
    let upstreams = {
        let mut health = app
            .health()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        upstreams(
            &snapshot.config,
            &mut health,
            Instant::now(),
            peers.expose_key_labels,
            peers.cooldown_bucket_secs,
            peers.expose_usage.then(|| app.pressure()),
        )
    };
    let upstream_total = snapshot
        .config
        .upstream
        .values()
        .filter(|upstream| !upstream.passthrough)
        .count();
    let observed_snapshot = app.observed().snapshot_at(app.wall_now());
    let (observed_snapshot, observed_truncated) = cap(observed_snapshot, MAX_OBSERVED);
    let mut doc = json!({
        "instance": instance_name(peers),
        "reiki_version": env!("CARGO_PKG_VERSION"),
        "observed_at": crate::reload::rfc3339(app.wall_now()),
        // Every figure is read as the answer is built; a reader adds its own time since receipt.
        "age_seconds": 0,
        "upstreams": upstreams,
        // Passthrough answers have no keys, so they live in their own list rather than pretending
        // to be keyed upstreams. Only operator labels and parsed header numbers are published.
        "observed": observed_snapshot
            .iter()
            .map(|entry| {
                serde_json::to_value(reiki_health::ObservedUpstream::from(entry))
                    .expect("an observed entry always serializes to JSON")
            })
            .collect::<Vec<_>>(),
        // Counts over the same bounded ring as `/v1/failures`; bodies and key names never leave.
        "failures": app.failures().counts(),
    });
    doc["upstreams_truncated"] = json!(upstream_total > MAX_UPSTREAMS);
    doc["observed_truncated"] = json!(observed_truncated);
    // Aggregate gateway counters: fixed names and counts only, gated by the same usage switch as
    // per-upstream traffic figures. No stream, model or key name ever reaches the wire.
    if peers.expose_usage {
        let counts = crate::gateway::GatewayCounts::sample(app.gateway());
        doc["gateway"] = json!({
            "requests": counts.requests,
            "tokens": counts.tokens,
            "store_errors": counts.store_errors,
        });
        let (static_streams, dynamic_streams) = app.stream_counts();
        doc["streams"] = json!({
            "static_streams": static_streams,
            "dynamic_streams": dynamic_streams,
        });
    }
    for (name, value) in app.plugins().health_gauges() {
        doc[name] = json!(value);
    }
    if peers.expose_host_stats {
        doc["host"] = host_stats(app, &snapshot.config).await;
        doc["sessions"] = sessions(app, peers);
    }
    doc
}

/// The directories Reiki writes to, by role: config and state, its own log, the request log.
pub(crate) fn disk_dirs(app: &App, config: &Config) -> Vec<(&'static str, std::path::PathBuf)> {
    let parent = |path: &std::path::Path| {
        path.parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map_or_else(
                || std::path::PathBuf::from("."),
                std::path::Path::to_path_buf,
            )
    };
    let mut dirs = Vec::new();
    if let Some(path) = app.config_path() {
        dirs.push(("config", parent(path)));
    }
    if let Some(path) = app.state_path() {
        dirs.push(("state", parent(path)));
    }
    if let Some(file) = &config.log.file {
        dirs.push(("log", parent(std::path::Path::new(file))));
    }
    if let Some(file) = config
        .plugins
        .get("requestlog")
        .and_then(|table| table.get("file"))
        .and_then(toml::Value::as_str)
    {
        dirs.push(("requestlog", parent(std::path::Path::new(file))));
    }
    dirs
}

/// Client-reported distinct session ids in the window: a total, and per stream only with
/// `expose_key_labels`, since stream names identify clients.
pub(crate) fn sessions(app: &App, peers: &PeersConfig) -> Value {
    let window = Duration::from_secs(peers.session_window_secs);
    let counts = app
        .peer_publisher()
        .sessions()
        .counts(window, Instant::now());
    let total: usize = counts.values().sum();
    let mut out = json!({
        "window_seconds": peers.session_window_secs,
        "active_sessions": total,
    });
    if peers.expose_key_labels {
        let (streams, truncated) = cap(counts.iter().collect::<Vec<_>>(), MAX_SESSION_STREAMS);
        out["streams"] = json!(
            streams
                .iter()
                .map(|(stream, count)| json!({"stream": stream, "active_sessions": count}))
                .collect::<Vec<_>>()
        );
        out["streams_truncated"] = json!(truncated);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::Policy;
    use crate::classify::FailureClass;

    fn config() -> Config {
        Config::parse(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n\
             [upstream.open]\nbase_url = \"https://example.com\"\npassthrough = true\n\
             [upstream.spare]\nbase_url = \"https://spare.example\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/a\"\n\
             [keys.b]\nupstream = \"zen\"\nfile = \"/b\"\n\
             [keys.c]\nupstream = \"zen\"\nfile = \"/c\"\n",
        )
        .unwrap()
    }

    #[test]
    fn every_published_array_reports_truncation_when_it_exceeds_its_bound() {
        for (total, max) in [
            (MAX_UPSTREAMS + 1, MAX_UPSTREAMS),
            (MAX_KEYS_PER_UPSTREAM + 1, MAX_KEYS_PER_UPSTREAM),
            (MAX_OBSERVED + 1, MAX_OBSERVED),
            (MAX_DISKS + 1, MAX_DISKS),
            (MAX_SESSION_STREAMS + 1, MAX_SESSION_STREAMS),
        ] {
            let (items, truncated) = cap((0..total).collect::<Vec<_>>(), max);
            assert_eq!(items.len(), max);
            assert!(
                truncated,
                "a bounded array of {total} must report truncation"
            );
            let (items, truncated) = cap((0..max).collect::<Vec<_>>(), max);
            assert_eq!(items.len(), max);
            assert!(
                !truncated,
                "an array at the bound must not report truncation"
            );
        }
    }

    #[test]
    fn upstream_and_key_arrays_stop_at_their_bounds() {
        let mut text = String::new();
        for index in 0..=MAX_UPSTREAMS {
            text.push_str(&format!(
                "[upstream.u{index}]\nbase_url = \"https://u{index}.example\"\n"
            ));
        }
        for index in 0..=MAX_KEYS_PER_UPSTREAM {
            text.push_str(&format!(
                "[keys.k{index}]\nupstream = \"u0\"\nfile = \"/dev/null\"\n"
            ));
        }
        let config = Config::parse(&text).unwrap();
        let mut health = KeyState::default();
        let out = upstreams(&config, &mut health, Instant::now(), true, 0, None);
        assert_eq!(out.len(), MAX_UPSTREAMS);
        assert_eq!(
            out[0]["keys"].as_array().unwrap().len(),
            MAX_KEYS_PER_UPSTREAM
        );
        assert_eq!(out[0]["keys_truncated"], true);
    }

    #[test]
    fn a_capacity_group_is_published_unsourced_and_key_labels_stay_labels() {
        let configured = Config::parse(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n\
             capacity_group = \"team-a\"\n\
             [keys.zen-a]\nupstream = \"zen\"\nfile = \"/a\"\n",
        )
        .unwrap();
        let mut health = KeyState::default();
        let zen = &upstreams(&configured, &mut health, Instant::now(), true, 0, None)[0];
        assert_eq!(zen["capacity_group"], "team-a");
        assert_eq!(zen["keys"][0]["name"], "zen-a");
        assert!(zen["keys"][0].get("capacity_group").is_none());

        let inferred = config();
        let zen = &upstreams(&inferred, &mut health, Instant::now(), true, 0, None)[1];
        assert!(zen.get("capacity_group").is_none(), "never inferred");
    }

    #[test]
    fn counts_each_state_and_the_soonest_cooldown() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        health.record_failure(
            "a",
            FailureClass::RateLimited(Some(Duration::from_millis(9_200))),
            &policy,
            now,
        );
        health.record_status("a", Some(429));
        health.disable("b");
        let out = upstreams(&config, &mut health, now, false, 0, None);
        assert_eq!(
            out.len(),
            2,
            "the passthrough upstream is left out: {out:?}"
        );
        let spare = &out[0];
        assert_eq!(spare["name"], "spare");
        assert_eq!(spare["healthy_keys"], 0);
        assert!(spare["soonest_cooldown_seconds"].is_null());
        assert!(spare["last_status_class"].is_null());
        let zen = &out[1];
        assert_eq!(zen["healthy_keys"], 1);
        assert_eq!(zen["cooling_keys"], 1);
        assert_eq!(zen["disabled_keys"], 1);
        assert_eq!(zen["soonest_cooldown_seconds"], 10, "rounded up");
        assert_eq!(zen["last_status_class"], "4xx");
        assert!(zen.get("keys").is_none(), "no labels by default");
    }

    #[test]
    fn labels_appear_only_when_asked_for() {
        let config = config();
        let mut health = KeyState::default();
        health.disable("c");
        let out = upstreams(&config, &mut health, Instant::now(), true, 0, None);
        let keys = out[1]["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(
            keys[2],
            json!({"name": "c", "state": "out_of_rotation", "cooldown_seconds": null})
        );
    }

    #[test]
    fn cooldowns_round_up_to_the_bucket_and_stay_exact_for_zero() {
        let secs = Duration::from_secs;
        for (wait, bucket, expected) in [
            (Duration::from_millis(1), 300, 300),
            (secs(1), 300, 300),
            (secs(299), 300, 300),
            (secs(300), 300, 300),
            (Duration::from_millis(300_001), 300, 600),
            (secs(301), 300, 600),
            (secs(601), 300, 900),
            (secs(7), 1, 7),
            (Duration::from_millis(9_200), 0, 10),
            (secs(42), 0, 42),
            (Duration::from_millis(1), 0, 1),
        ] {
            assert_eq!(ceil_secs(wait, bucket), expected, "{wait:?} in {bucket}s");
        }
    }

    #[test]
    fn bucketing_applies_to_the_soonest_and_every_labelled_cooldown() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        for (key, millis) in [("a", 9_200), ("b", 301_000)] {
            health.record_failure(
                key,
                FailureClass::RateLimited(Some(Duration::from_millis(millis))),
                &policy,
                now,
            );
        }
        let zen = &upstreams(&config, &mut health, now, true, 300, None)[1];
        assert_eq!(zen["soonest_cooldown_seconds"], 300);
        assert_eq!(zen["keys"][0]["cooldown_seconds"], 300);
        assert_eq!(zen["keys"][1]["cooldown_seconds"], 600);
        let exact = &upstreams(&config, &mut health, now, true, 0, None)[1];
        assert_eq!(exact["soonest_cooldown_seconds"], 10);
        assert_eq!(exact["keys"][1]["cooldown_seconds"], 301);
    }

    #[test]
    fn cooling_combinations_count_scoped_cooldowns_and_no_names() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        let scope = |route: &str, model: &str| crate::chain::Scope {
            route: route.to_owned(),
            model: model.to_owned(),
        };
        for (route, model) in [("/chat/completions", "model-x"), ("/responses", "model-y")] {
            health.record_failure_scoped(
                "a",
                FailureClass::RateLimited(Some(Duration::from_secs(10))),
                &policy,
                now,
                None,
                Some(&scope(route, model)),
                3,
            );
        }
        // Labelled or not, the count is there and no route or model text leaves the host.
        for labels in [false, true] {
            let out = upstreams(&config, &mut health, now, labels, 0, None);
            assert_eq!(out[1]["cooling_combinations"], 2, "labels={labels}");
            assert_eq!(
                out[0]["cooling_combinations"], 0,
                "an upstream with no keys"
            );
            let text = serde_json::to_string(&out).unwrap();
            for (i, secret) in [
                "model-x",
                "model-y",
                "/chat",
                "/responses",
                "route",
                "model",
            ]
            .into_iter()
            .enumerate()
            {
                assert!(
                    !text.contains(secret),
                    "secret #{i} leaked into the health document"
                );
            }
        }
    }

    #[test]
    fn a_whole_key_cooldown_counts_no_combinations() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        let paid = crate::chain::Scope {
            route: "/chat/completions".to_owned(),
            model: "model-x".to_owned(),
        };
        health.record_failure_scoped(
            "a",
            FailureClass::RateLimited(Some(Duration::from_secs(10))),
            &policy,
            now,
            None,
            Some(&paid),
            3,
        );
        // A failure with no scope cools the whole key, which clears its combinations.
        health.record_failure(
            "a",
            FailureClass::RateLimited(Some(Duration::from_secs(10))),
            &policy,
            now,
        );
        let zen = &upstreams(&config, &mut health, now, false, 0, None)[1];
        assert_eq!(zen["cooling_keys"], 1);
        assert_eq!(zen["cooling_combinations"], 0, "{zen}");
    }

    #[test]
    fn labels_name_the_cooldown_class_of_a_cooling_key_only() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        health.record_failure("a", FailureClass::QuotaExhausted, &policy, now);
        health.disable("c");
        let zen = &upstreams(&config, &mut health, now, true, 0, None)[1];
        assert_eq!(zen["keys"][0]["cooldown_class"], "quota_exhausted");
        assert!(
            zen["keys"][1].get("cooldown_class").is_none(),
            "b is available"
        );
        assert!(
            zen["keys"][2].get("cooldown_class").is_none(),
            "c is out of rotation"
        );
    }

    #[test]
    fn a_quota_reset_is_published_only_for_a_labelled_quota_key() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        let reset = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(86_400);
        health.record_failure_with("a", FailureClass::QuotaExhausted, &policy, now, None);
        health.note_quota_reset("a", reset, now);
        health.record_failure(
            "b",
            FailureClass::RateLimited(Some(Duration::from_secs(10))),
            &policy,
            now,
        );

        let zen = &upstreams(&config, &mut health, now, true, 0, None)[1];
        assert_eq!(zen["keys"][0]["quota_reset_at"], "1970-01-02T00:00:00Z");
        assert!(zen["keys"][1].get("quota_reset_at").is_none());
        let unlabelled = &upstreams(&config, &mut health, now, false, 0, None)[1];
        assert!(!unlabelled.to_string().contains("quota_reset_at"));
    }

    #[test]
    fn usage_fields_appear_only_when_exposed() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let pressure = PressureCounter::new();
        pressure.record("zen", false, now);
        pressure.record("zen", true, now);
        pressure.record("spare", false, now);
        pressure.record_tokens("zen", 30, now);
        let exposed = upstreams(&config, &mut health, now, false, 0, Some(&pressure));
        let zen = &exposed[1];
        assert_eq!(zen["requests_1h"], 2);
        assert_eq!(zen["limited_1h"], 1);
        assert_eq!(zen["tokens_1h"], 30);
        let buckets = zen["request_buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 12);
        assert_eq!(buckets[11], 2, "the newest bucket holds both attempts");
        let token_buckets = zen["token_buckets"].as_array().unwrap();
        assert_eq!(token_buckets.len(), 12);
        assert_eq!(token_buckets[11], 30);
        assert_eq!(exposed[0]["requests_1h"], 1, "per upstream, not pooled");
        assert_eq!(exposed[0]["tokens_1h"], 0);

        let hidden = upstreams(&config, &mut health, now, false, 0, None);
        for entry in &hidden {
            for field in [
                "requests_1h",
                "limited_1h",
                "request_buckets",
                "tokens_1h",
                "token_buckets",
            ] {
                assert!(entry.get(field).is_none(), "{field} is hidden: {entry}");
            }
        }
    }

    #[test]
    fn the_unlabelled_view_never_carries_a_cooldown_class() {
        let config = config();
        let mut health = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        health.record_failure("a", FailureClass::QuotaExhausted, &policy, now);
        let zen = &upstreams(&config, &mut health, now, false, 0, None)[1];
        let fields: Vec<&str> = zen
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            fields,
            [
                "cooling_combinations",
                "cooling_keys",
                "disabled_keys",
                "healthy_keys",
                "keys_truncated",
                "last_status_class",
                "name",
                "soonest_cooldown_seconds"
            ]
        );
        assert!(!zen.to_string().contains("cooldown_class"));
    }

    #[tokio::test]
    async fn the_document_carries_failure_counts_but_never_bodies_or_key_names() {
        let text = concat!(
            "[upstream.zen]\nbase_url = \"https://example.com\"\n",
            "[keys.secret-key-name]\nupstream = \"zen\"\nfile = \"/a\"\n",
            "[peers]\n",
        );
        let config = Config::parse(text).unwrap();
        let app = crate::proxy::App::with_header_timeout(
            config,
            std::collections::HashMap::new(),
            Policy::default(),
            Duration::from_secs(1),
        );
        for class in [
            "rate_limited",
            "rate_limited",
            "transient",
            "request_error",
            "not_a_class",
        ] {
            app.failures().record(crate::failures::FailureRecord {
                time: 1,
                stream: "stream-name".to_owned(),
                key: "secret-key-name".to_owned(),
                status: Some(429),
                class: class.to_owned(),
                retry_after: Some("120".to_owned()),
                action: crate::failures::FailureAction::Cooldown,
                cooled: true,
                cooldown_seconds: Some(10),
                note: Some("a body with secret-key-name in it".to_owned()),
            });
        }
        let peers = app.snapshot().config.peers.clone().unwrap();
        let doc = document(&app, &peers).await;
        assert_eq!(doc["failures"]["rate_limited"], 2);
        assert_eq!(doc["failures"]["transient"], 1);
        assert_eq!(doc["failures"]["request_error"], 1);
        assert!(doc["failures"].get("not_a_class").is_none());
        let text = doc.to_string();
        for (i, hidden) in ["secret-key-name", "stream-name", "a body", "120"]
            .into_iter()
            .enumerate()
        {
            assert!(
                !text.contains(hidden),
                "hidden value #{i} leaked into the health document"
            );
        }
    }

    #[tokio::test]
    async fn gateway_counts_are_published_as_names_only_counts_when_exposure_is_on() {
        let text = concat!(
            "[upstream.zen]\nbase_url = \"https://example.com\"\n",
            "[keys.k]\nupstream = \"zen\"\nfile = \"/unused\"\n",
            "[streams.tenant]\ntoken_sha256 = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nchain = [\"k\"]\n",
            "[peers]\n",
        );
        let config = Config::parse(text).unwrap();
        let app = crate::proxy::App::new(
            config,
            [("k".to_owned(), crate::proxy::Secret::new("secret"))].into(),
            Policy::default(),
            crate::plugins::PluginChain::default(),
        );
        let snapshot = app.snapshot();
        let admitted = app
            .gateway()
            .admit(
                &snapshot.config,
                "tenant",
                "zen",
                "k",
                Some("secret-model"),
                app.wall_now(),
                17,
            )
            .expect("admission succeeds");
        drop(admitted);
        let peers = snapshot.config.peers.clone().unwrap();
        let doc = document(&app, &peers).await;
        assert_eq!(doc["gateway"]["requests"], 1);
        assert_eq!(doc["gateway"]["tokens"], 17);
        assert_eq!(doc["gateway"]["store_errors"], 0);
        let body = doc.to_string();
        for hidden in ["tenant", "secret-model", "secret"] {
            assert!(
                !body.contains(hidden),
                "{hidden} leaked into the health document"
            );
        }

        let hidden = Config::parse(&format!("{text}expose_usage = false\n")).unwrap();
        let app = crate::proxy::App::new(
            hidden,
            [("k".to_owned(), crate::proxy::Secret::new("secret"))].into(),
            Policy::default(),
            crate::plugins::PluginChain::default(),
        );
        let peers = app.snapshot().config.peers.clone().unwrap();
        let doc = document(&app, &peers).await;
        assert!(
            doc.get("gateway").is_none(),
            "counts are gated by expose_usage"
        );
    }

    #[test]
    fn the_instance_defaults_to_a_fixed_name_not_the_host_name() {
        let parse = |text: &str| Config::parse(text).unwrap().peers.expect("a [peers] table");
        assert_eq!(instance_name(&parse("[peers]\n")), "reiki");
        assert_eq!(
            instance_name(&parse("[peers]\ninstance = \"peer-a\"\n")),
            "peer-a"
        );
    }

    #[test]
    fn status_classes_are_coarse() {
        assert_eq!(status_class(Some(201)).as_deref(), Some("2xx"));
        assert_eq!(status_class(Some(404)).as_deref(), Some("4xx"));
        assert_eq!(status_class(Some(503)).as_deref(), Some("5xx"));
        assert_eq!(status_class(Some(302)), None);
        assert_eq!(status_class(None), None);
    }

    fn validate_schema(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
        if let Some(options) = schema.get("enum").and_then(Value::as_array)
            && !options.iter().any(|option| option == value)
        {
            return Err(format!("{path}: value {value} not in enum {options:?}"));
        }
        if let Some(types) = schema.get("type") {
            let ok = match types {
                Value::String(t) => type_matches(value, t),
                Value::Array(items) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|t| type_matches(value, t)),
                _ => false,
            };
            if !ok {
                return Err(format!("{path}: {value} does not match type {types}"));
            }
        }
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && let Some(n) = value.as_f64()
            && n < min
        {
            return Err(format!("{path}: {n} < minimum {min}"));
        }
        if let Value::Object(obj) = value {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !obj.contains_key(name) {
                        return Err(format!("{path}: missing required {name}"));
                    }
                }
            }
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (name, prop_schema) in props {
                    if let Some(child) = obj.get(name) {
                        validate_schema(child, prop_schema, &format!("{path}.{name}"))?;
                    }
                }
            }
            let additional = schema
                .get("additionalProperties")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !additional && let Some(props) = schema.get("properties").and_then(Value::as_object)
            {
                for name in obj.keys() {
                    if !props.contains_key(name) {
                        return Err(format!("{path}: unexpected property {name}"));
                    }
                }
            }
        }
        if let Value::Array(items) = value {
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < min
            {
                return Err(format!("{path}: {} items < minItems {min}", items.len()));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
                && (items.len() as u64) > max
            {
                return Err(format!("{path}: {} items > maxItems {max}", items.len()));
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate_schema(item, item_schema, &format!("{path}[{index}]"))?;
                }
            }
        }
        Ok(())
    }

    fn type_matches(value: &Value, ty: &str) -> bool {
        match ty {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        }
    }

    #[tokio::test]
    async fn a_real_health_document_matches_the_generated_schema() {
        let config = Config::parse(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/a\"\n\
             [peers]\nexpose_usage = true\nexpose_host_stats = true\nexpose_key_labels = true\n",
        )
        .unwrap();
        let app = crate::proxy::App::new(
            config,
            [("a".to_owned(), crate::proxy::Secret::new("secret"))].into(),
            Policy::default(),
            crate::plugins::PluginChain::default(),
        );
        let peers = app.snapshot().config.peers.clone().unwrap();
        let doc = document(&app, &peers).await;
        let schema = crate::peers::openapi::health_schema();
        validate_schema(&doc, &schema, "$").unwrap();

        let base = Config::parse(
            "[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n\
             [keys.a]\nupstream = \"zen\"\nfile = \"/a\"\n\
             [peers]\n",
        )
        .unwrap();
        let app = crate::proxy::App::new(
            base,
            [("a".to_owned(), crate::proxy::Secret::new("secret"))].into(),
            Policy::default(),
            crate::plugins::PluginChain::default(),
        );
        let peers = app.snapshot().config.peers.clone().unwrap();
        let doc = document(&app, &peers).await;
        validate_schema(&doc, &schema, "$").unwrap();
    }
}
