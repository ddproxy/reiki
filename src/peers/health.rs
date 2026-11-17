//! The `/v1/health` document: per-upstream key counts, never key material, file paths, stream
//! names or request volume.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::chain::{KeyState, KeyStatus};
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

fn status_class(status: Option<u16>) -> Value {
    match status {
        Some(200..=299) => json!("2xx"),
        Some(400..=499) => json!("4xx"),
        Some(500..=599) => json!("5xx"),
        _ => Value::Null,
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
                let mut item = json!({"name": key, "state": state, "cooldown_seconds": wait});
                if wait.is_some() {
                    if let Some(class) = health.cooling_class(key, now) {
                        item["cooldown_class"] = json!(class.name());
                    }
                }
                listed.push(item);
            }
        }
        let mut entry = json!({
            "name": name,
            "healthy_keys": healthy,
            "cooling_keys": cooling,
            "disabled_keys": disabled,
            "cooling_combinations": combinations,
            "soonest_cooldown_seconds": soonest.map(|wait| ceil_secs(wait, bucket)),
            "last_status_class": health
                .latest_status(keys.iter().copied())
                .map_or(Value::Null, status_class),
        });
        if let Some(pressure) = pressure {
            let (requests, limited) = pressure.totals(name, now);
            entry["requests_1h"] = json!(requests);
            entry["limited_1h"] = json!(limited);
            entry["request_buckets"] = json!(pressure.buckets(name, now));
            entry["tokens_1h"] = json!(pressure.token_totals(name, now));
            entry["token_buckets"] = json!(pressure.token_buckets(name, now));
            entry["request_minutes"] = json!(pressure.minutes(name, now));
            entry["token_minutes"] = json!(pressure.token_minutes(name, now));
        }
        if labels {
            entry["keys"] = Value::Array(listed);
        }
        out.push(entry);
    }
    out
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
    let mut doc = json!({
        "instance": instance_name(peers),
        "reiki_version": env!("CARGO_PKG_VERSION"),
        "observed_at": crate::reload::rfc3339(app.wall_now()),
        // Every figure is read as the answer is built; a reader adds its own time since receipt.
        "age_seconds": 0,
        "upstreams": upstreams,
        // Counts over the same bounded ring as `/v1/failures`; bodies and key names never leave.
        "failures": app.failures().counts(),
    });
    if peers.expose_host_stats {
        doc["host"] = super::host::stats(&disk_dirs(app, &snapshot.config)).await;
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
        out["streams"] = counts
            .iter()
            .map(|(stream, count)| json!({"stream": stream, "active_sessions": count}))
            .collect();
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
            for secret in [
                "model-x",
                "model-y",
                "/chat",
                "/responses",
                "route",
                "model",
            ] {
                assert!(!text.contains(secret), "`{secret}` leaked: {text}");
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
        for class in ["rate_limited", "rate_limited", "transient"] {
            app.failures().record(crate::failures::FailureRecord {
                time: 1,
                stream: "stream-name".to_owned(),
                key: "secret-key-name".to_owned(),
                status: Some(429),
                class: class.to_owned(),
                retry_after: Some("120".to_owned()),
                cooled: true,
                cooling_seconds: Some(10),
                note: "a body with secret-key-name in it".to_owned(),
            });
        }
        let peers = app.snapshot().config.peers.clone().unwrap();
        let doc = document(&app, &peers).await;
        assert_eq!(doc["failures"]["rate_limited"], 2);
        assert_eq!(doc["failures"]["transient"], 1);
        let text = doc.to_string();
        for hidden in ["secret-key-name", "stream-name", "a body", "120"] {
            assert!(!text.contains(hidden), "`{hidden}` leaked: {text}");
        }
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
        assert_eq!(status_class(Some(201)), "2xx");
        assert_eq!(status_class(Some(404)), "4xx");
        assert_eq!(status_class(Some(503)), "5xx");
        assert!(status_class(Some(302)).is_null());
        assert!(status_class(None).is_null());
    }
}
