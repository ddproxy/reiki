use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::chain::SavedHealth;
use crate::config::Config;
use crate::proxy::Secret;

/// Longest cooldown a state file may restore. Quota resets can be monthly, so
/// the cap is a year; it only guards against a hand-edited recovery time that
/// would overflow `Instant`.
const MAX_RESTORED_COOLDOWN: Duration = Duration::from_secs(366 * 24 * 60 * 60);

/// Sits next to the config file. Records key health only (names, cooldowns, disabled flag,
/// failures, key SHA-256); never a key value.
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
        if secret_changed(&key.file, saved_at, entry.key_sha256.as_deref()) {
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

pub fn save(path: &Path, snapshot: &HashMap<String, SavedHealth>, keys: &HashMap<String, Secret>) {
    let now = unix_millis(SystemTime::now());
    let entries = snapshot
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
                    key_sha256: keys
                        .get(name)
                        .map(|key| crate::auth::sha256_hex(key.expose())),
                },
            )
        })
        .collect();
    let file = File {
        saved_at: now,
        keys: entries,
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

/// A key whose secret changed after the state was written starts healthy. The recorded digest
/// decides when there is one; state without it falls back to the file's modified time. A file
/// that cannot be read counts as changed: nothing proves it holds the recorded secret.
fn secret_changed(file: &str, saved_at: SystemTime, saved_digest: Option<&str>) -> bool {
    if let Some(saved) = saved_digest {
        let digest = std::fs::read_to_string(file)
            .ok()
            .map(|text| crate::auth::sha256_hex(text.trim()));
        return digest.as_deref() != Some(saved);
    }
    std::fs::metadata(file)
        .and_then(|meta| meta.modified())
        .map_or(true, |modified| modified > saved_at)
}

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write to a sibling temporary file, then rename over the target and sync the directory, so a
/// reader never sees a partial file and the rename survives a crash.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let mut attempts = 0;
    let (temp, mut file) = loop {
        let mut name: OsString = path.as_os_str().to_owned();
        let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        name.push(format!(".tmp.{}.{n}", std::process::id()));
        let temp = PathBuf::from(name);
        // A file already at the temporary path is never followed or reused.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < 8 => attempts += 1,
            Err(e) => return Err(e),
        }
    };
    let written = std::io::Write::write_all(&mut file, text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
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
    /// SHA-256 of the key when saved; absent in state written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_sha256: Option<String>,
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

    /// A key file last modified an hour ago, so state saved now is newer than it.
    fn old_key_file(tag: &str) -> PathBuf {
        let path = temp_path(&format!("{tag}-key"));
        std::fs::write(&path, "sk-fake-secret").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        path
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
                    key_sha256: None,
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
        let (key_a, key_b) = (old_key_file("rt-a"), old_key_file("rt-b"));
        let config = config_with_keys(&[
            ("a", key_a.to_str().unwrap()),
            ("b", key_b.to_str().unwrap()),
        ]);
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

        save(
            &path,
            &state.snapshot(now),
            &crate::proxy::load_keys(&config).unwrap(),
        );
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
    fn the_state_file_holds_a_key_digest_and_never_the_key() {
        let path = temp_path("secrets");
        let _ = std::fs::remove_file(&path);
        let key_file = old_key_file("secrets");
        std::fs::write(&key_file, "sk-fake-secret\n").unwrap();
        let mut state = KeyState::default();
        state.record_failure(
            "a",
            FailureClass::Unauthorized,
            &Policy::default(),
            Instant::now(),
        );
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        save(
            &path,
            &state.snapshot(Instant::now()),
            &crate::proxy::load_keys(&config).unwrap(),
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-fake-secret"));
        assert!(text.contains(&crate::auth::sha256_hex("sk-fake-secret")));
        assert!(!text.contains(key_file.to_str().unwrap()));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn an_absurd_recovery_time_is_capped_instead_of_overflowing() {
        let path = temp_path("absurd");
        let key_file = old_key_file("absurd");
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
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
        let key_file = old_key_file("month");
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        let now = unix_millis(SystemTime::now());
        let in_20_days = now + 20 * 86_400_000;
        write_state(&path, now, &[("a", false, in_20_days, 0)]);

        let cooling = load(&path, &config)["a"].cooling.unwrap();
        assert!(cooling > Duration::from_secs(19 * 86_400));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_key_file_that_cannot_be_read_starts_healthy() {
        let path = temp_path("unreadable");
        let config = config_with_keys(&[("a", "/unused/a")]);
        let now = unix_millis(SystemTime::now());
        write_state(&path, now, &[("a", true, now + 3_600_000, 3)]);
        assert!(load(&path, &config).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_replaced_key_starts_healthy_even_with_an_old_mtime() {
        let path = temp_path("resized");
        let key_file = old_key_file("resized");
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        let mut state = KeyState::default();
        state.record_failure(
            "a",
            FailureClass::Unauthorized,
            &Policy::default(),
            Instant::now(),
        );
        save(
            &path,
            &state.snapshot(Instant::now()),
            &crate::proxy::load_keys(&config).unwrap(),
        );
        assert!(load(&path, &config)["a"].disabled, "same file is restored");

        std::fs::write(&key_file, "a-longer-replacement-secret").unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&key_file)
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        assert!(load(&path, &config).is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn saving_survives_a_planted_temporary_file_and_leaves_none_behind() {
        let dir = std::env::temp_dir().join(format!("reiki-state-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reiki.toml.state");
        let planted = dir.join("planted");
        std::fs::write(&planted, "x").unwrap();
        #[cfg(unix)]
        for n in 0..TEMP_COUNTER.load(std::sync::atomic::Ordering::Relaxed) + 3 {
            let name = format!("reiki.toml.state.tmp.{}.{n}", std::process::id());
            std::os::unix::fs::symlink(&planted, dir.join(name)).unwrap();
        }
        write_atomically(&path, "saved_at = 1\n").unwrap();
        write_atomically(&path, "saved_at = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "saved_at = 2\n");
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
