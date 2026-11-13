//! The `/v1/health` document: per-upstream key counts, never key material, file paths, stream
//! names or request volume.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::chain::{KeyState, KeyStatus};
use crate::config::{Config, PeersConfig};
use crate::proxy::App;

/// Whole seconds, rounded up so a short wait never reads as zero.
fn ceil_secs(wait: Duration) -> u64 {
    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1)
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
    health: &KeyState,
    now: Instant,
    labels: bool,
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
        let mut soonest: Option<Duration> = None;
        let mut listed = Vec::new();
        for key in &keys {
            let (state, wait) = match health.status(key, now) {
                KeyStatus::Available => {
                    healthy += 1;
                    ("available", None)
                }
                KeyStatus::Cooling(wait) => {
                    cooling += 1;
                    soonest = Some(soonest.map_or(wait, |s| s.min(wait)));
                    ("cooling", Some(ceil_secs(wait)))
                }
                KeyStatus::OutOfRotation => {
                    disabled += 1;
                    ("out_of_rotation", None)
                }
            };
            if labels {
                listed.push(json!({"name": key, "state": state, "cooldown_seconds": wait}));
            }
        }
        let mut entry = json!({
            "name": name,
            "healthy_keys": healthy,
            "cooling_keys": cooling,
            "disabled_keys": disabled,
            "soonest_cooldown_seconds": soonest.map(ceil_secs),
            "last_status_class": health
                .latest_status(keys.iter().copied())
                .map_or(Value::Null, status_class),
        });
        if labels {
            entry["keys"] = Value::Array(listed);
        }
        out.push(entry);
    }
    out
}

/// The name this instance publishes: the configured one, or the host name.
pub(crate) fn instance_name(peers: &PeersConfig) -> String {
    if let Some(name) = &peers.instance {
        return name.clone();
    }
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|name| !name.is_empty())
        })
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "reiki".to_owned())
}

/// The whole `/v1/health` answer for the current snapshot and key health.
pub(crate) fn document(app: &App, peers: &PeersConfig) -> Value {
    let snapshot = app.snapshot();
    let upstreams = {
        let health = app
            .health()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        upstreams(
            &snapshot.config,
            &health,
            Instant::now(),
            peers.expose_key_labels,
        )
    };
    json!({
        "instance": instance_name(peers),
        "observed_at": crate::reload::rfc3339(app.wall_now()),
        "upstreams": upstreams,
    })
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
        let out = upstreams(&config, &health, now, false);
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
        let out = upstreams(&config, &health, Instant::now(), true);
        let keys = out[1]["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 3);
        assert_eq!(
            keys[2],
            json!({"name": "c", "state": "out_of_rotation", "cooldown_seconds": null})
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
