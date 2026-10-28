use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::chain::SavedHealth;
use crate::config::Config;

/// Longest cooldown a state file may restore. Quota resets can be monthly, so
/// the cap is a year; it only guards against a hand-edited recovery time that
/// would overflow `Instant`.
const MAX_RESTORED_COOLDOWN: Duration = Duration::from_secs(366 * 24 * 60 * 60);

/// The state file sits next to the config file. It records key health only:
/// names, cooldowns, the disabled flag and consecutive failures. It never holds
/// a key value or a hash of one, so a changed secret is detected from the key
/// file's modified time instead.
pub fn path_for(config_path: &Path) -> PathBuf {
    let mut name: OsString = config_path.as_os_str().to_owned();
    name.push(".state");
    PathBuf::from(name)
}

pub fn load(path: &Path, config: &Config) -> HashMap<String, SavedHealth> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read key state; starting fresh");
            return HashMap::new();
        }
    };
    let file: File = match toml::from_str(&text) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ignoring corrupt key state");
            return HashMap::new();
        }
    };

    let now = unix_millis(SystemTime::now());
    let saved_at = UNIX_EPOCH + Duration::from_millis(file.saved_at);
    let mut out = HashMap::new();
    for (name, entry) in file.keys {
        let Some(key) = config.keys.get(&name) else {
            continue;
        };
        if secret_changed(&key.file, saved_at) {
            continue;
        }
        let cooling = if entry.recovery > now {
            Some(Duration::from_millis(entry.recovery - now).min(MAX_RESTORED_COOLDOWN))
        } else {
            None
        };
        out.insert(
            name,
            SavedHealth {
                disabled: entry.disabled,
                cooling,
                transient_failures: entry.failures,
            },
        );
    }
    out
}

pub fn save(path: &Path, snapshot: &HashMap<String, SavedHealth>) {
    let now = unix_millis(SystemTime::now());
    let keys = snapshot
        .iter()
        .map(|(name, health)| {
            let recovery = health
                .cooling
                .map_or(0, |cooling| now + cooling.as_millis() as u64);
            (
                name.clone(),
                Entry {
                    disabled: health.disabled,
                    recovery,
                    failures: health.transient_failures,
                },
            )
        })
        .collect();
    let file = File {
        saved_at: now,
        keys,
    };
    let text = match toml::to_string(&file) {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(error = %e, "cannot serialise key state");
            return;
        }
    };
    if let Err(e) = write_atomically(path, &text) {
        tracing::warn!(path = %path.display(), error = %e, "cannot write key state");
    }
}

/// A key whose key file changed after the state was written holds a new secret,
/// so its saved health no longer applies and it starts healthy.
fn secret_changed(file: &str, saved_at: SystemTime) -> bool {
    std::fs::metadata(file)
        .and_then(|meta| meta.modified())
        .map(|modified| modified > saved_at)
        .unwrap_or(false)
}

/// Write to a sibling temporary file, then rename over the target, so a reader
/// never sees a partial file.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(format!(".tmp.{}", std::process::id()));
    let temp = PathBuf::from(name);
    // A stale or planted file at the temporary path is replaced, never followed or reused.
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temp, path)
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Serialize, Deserialize)]
struct File {
    saved_at: u64,
    #[serde(default)]
    keys: BTreeMap<String, Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    recovery: u64,
    #[serde(default)]
    failures: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{KeyState, Policy};
    use crate::classify::FailureClass;
    use std::time::Instant;

    fn config_with_keys(files: &[(&str, &str)]) -> Config {
        let mut text = String::from("[upstream.zen]\nbase_url = \"https://opencode.ai/zen/v1\"\n");
        for (name, file) in files {
            text.push_str(&format!(
                "[keys.{name}]\nupstream = \"zen\"\nfile = \"{file}\"\n"
            ));
        }
        Config::parse(&text).unwrap()
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("reiki-state-{}-{tag}.toml", std::process::id()))
    }

    fn write_state(path: &Path, saved_at: u64, entries: &[(&str, bool, u64, u32)]) {
        let mut keys = BTreeMap::new();
        for (name, disabled, recovery, failures) in entries {
            keys.insert(
                (*name).to_owned(),
                Entry {
                    disabled: *disabled,
                    recovery: *recovery,
                    failures: *failures,
                },
            );
        }
        let file = File { saved_at, keys };
        write_atomically(path, &toml::to_string(&file).unwrap()).unwrap();
    }

    #[test]
    fn key_health_round_trips_through_the_file() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let config = config_with_keys(&[("a", "/unused/a"), ("b", "/unused/b")]);
        let mut state = KeyState::default();
        let now = Instant::now();
        let policy = Policy::default();
        state.record_failure(
            "a",
            FailureClass::RateLimited(Some(Duration::from_secs(50))),
            &policy,
            now,
        );
        state.record_failure("b", FailureClass::Unauthorized, &policy, now);

        save(&path, &state.snapshot(now));
        let loaded = load(&path, &config);

        assert!(loaded["a"].cooling.unwrap() > Duration::from_secs(40));
        assert!(!loaded["a"].disabled);
        assert_eq!(loaded["b"].transient_failures, 0);
        assert!(loaded["b"].disabled);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_loads_as_empty() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let config = config_with_keys(&[("a", "/unused/a")]);
        assert!(load(&path, &config).is_empty());
    }

    #[test]
    fn a_corrupt_file_loads_as_empty() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "saved_at = \"soon\"\n[keys.\n").unwrap();
        let config = config_with_keys(&[("a", "/unused/a")]);
        assert!(load(&path, &config).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_key_file_newer_than_the_state_starts_healthy() {
        let path = temp_path("changed");
        let key_file = temp_path("changed-key");
        std::fs::write(&key_file, "sk-fake-secret").unwrap();
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        write_state(&path, 1_000, &[("a", true, 4_000_000_000_000, 3)]);
        assert!(load(&path, &config).is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn an_unchanged_key_is_restored() {
        let path = temp_path("unchanged");
        let key_file = temp_path("unchanged-key");
        std::fs::write(&key_file, "sk-fake-secret").unwrap();
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        let now = unix_millis(SystemTime::now());
        write_state(&path, now + 3_600_000, &[("a", false, now + 3_600_000, 2)]);
        let loaded = load(&path, &config);
        assert_eq!(loaded.len(), 1);
        assert!(loaded["a"].cooling.is_some());
        assert_eq!(loaded["a"].transient_failures, 2);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn the_state_file_never_holds_a_key_value_or_hash() {
        let path = temp_path("secrets");
        let _ = std::fs::remove_file(&path);
        let mut state = KeyState::default();
        state.record_failure(
            "a",
            FailureClass::Unauthorized,
            &Policy::default(),
            Instant::now(),
        );
        save(&path, &state.snapshot(Instant::now()));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-fake-secret"));
        assert!(!text.contains(&crate::auth::sha256_hex("sk-fake-secret")));
        assert!(!text.contains("/unused/"));
        assert!(text.contains("a"));
        let _ = std::fs::remove_file(&path);
    }
    #[test]
    fn an_absurd_recovery_time_is_capped_instead_of_overflowing() {
        let path = temp_path("absurd");
        let config = config_with_keys(&[("a", "/unused/a")]);
        let now = unix_millis(SystemTime::now());
        write_state(&path, now, &[("a", false, i64::MAX as u64, 0)]);

        let loaded = load(&path, &config);
        let cooling = loaded["a"].cooling.unwrap();
        assert_eq!(cooling, MAX_RESTORED_COOLDOWN);

        // Anchoring the capped value to the monotonic clock must not panic.
        let mut state = KeyState::default();
        state.restore("a", loaded["a"], Instant::now());
        assert!(state.cooling_remaining("a", Instant::now()).is_some());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_month_long_quota_cooldown_survives_a_restart() {
        let path = temp_path("month");
        let config = config_with_keys(&[("a", "/unused/a")]);
        let now = unix_millis(SystemTime::now());
        let in_20_days = now + 20 * 86_400_000;
        write_state(&path, now, &[("a", false, in_20_days, 0)]);

        let cooling = load(&path, &config)["a"].cooling.unwrap();
        assert!(cooling > Duration::from_secs(19 * 86_400));
        let _ = std::fs::remove_file(&path);
    }
}
