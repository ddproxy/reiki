use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::hash::{BuildHasher, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::chain::SavedHealth;
use crate::config::Config;
use crate::proxy::Secret;
use crate::shaper::bucket::{BucketSnapshot, QuotaSnapshot};

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

/// Probes a directory by creating and removing a file, so the daemon fails before binding
/// listeners instead of silently losing state later.
pub fn check_writable(dir: &Path) -> Result<(), String> {
    let name = format!(".reiki-probe-{}-{:016x}", std::process::id(), probe_nonce());
    probe_file(dir, &name)
}

/// Nonce for the probe name, so a squatter cannot plant a symlink at a predictable path.
fn probe_nonce() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    hasher.write_u64(std::process::id() as u64);
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.finish()
}

/// `name` is separate from [`check_writable`] so tests can plant a file or symlink at it.
fn probe_file(dir: &Path, name: &str) -> Result<(), String> {
    let probe = dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    // A pre-existing file is never reused: it may be a symlink to something the caller
    // should not truncate, or a victim file a local attacker wants removed.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    let failure =
        |e: std::io::Error| format!("state directory {} is not writable: {e}", dir.display());
    options
        .open(&probe)
        .map_err(failure)
        .and_then(|_| std::fs::remove_file(&probe).map_err(failure))
}

#[derive(Debug, Default)]
pub struct LoadedState {
    pub health: HashMap<String, SavedHealth>,
    pub shaper: BTreeMap<String, BucketSnapshot>,
}

pub fn load(path: &Path, config: &Config) -> HashMap<String, SavedHealth> {
    load_state(path, config).health
}

pub fn load_shaper(path: &Path, config: &Config) -> BTreeMap<String, BucketSnapshot> {
    load_state(path, config).shaper
}

pub fn load_state(path: &Path, config: &Config) -> LoadedState {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return LoadedState::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read key state; starting fresh");
            return LoadedState::default();
        }
    };
    let file: File = match toml::from_str(&text) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ignoring corrupt key state");
            return LoadedState::default();
        }
    };

    let now = unix_millis(SystemTime::now());
    let saved_at = UNIX_EPOCH + Duration::from_millis(file.saved_at);
    let mut out = LoadedState::default();
    for (name, entry) in &file.keys {
        if !matches_key(config, name, saved_at, &entry.upstream, &entry.key_sha256) {
            continue;
        }
        let cooling = if entry.recovery > now {
            Some(Duration::from_millis(entry.recovery - now).min(MAX_RESTORED_COOLDOWN))
        } else {
            None
        };
        out.health.insert(
            name.clone(),
            SavedHealth {
                disabled: entry.disabled,
                cooling,
                transient_failures: entry.failures,
            },
        );
    }
    for (name, entry) in file.shaper {
        if !matches_key(config, &name, saved_at, &entry.upstream, &entry.key_sha256) {
            continue;
        }
        let snapshot = entry.into_snapshot();
        if !snapshot.is_empty() {
            out.shaper.insert(name, snapshot);
        }
    }
    out
}

/// The existing rotation rule: same configured key, same upstream and same key-file
/// digest (or an older file than the state) as when the state was written.
fn matches_key(
    config: &Config,
    name: &str,
    saved_at: SystemTime,
    saved_upstream: &Option<String>,
    saved_digest: &Option<String>,
) -> bool {
    let Some(key) = config.keys.get(name) else {
        return false;
    };
    // A re-pointed key (same name and secret, different upstream) must start healthy.
    if let Some(saved_upstream) = saved_upstream.as_deref() {
        if saved_upstream != key.upstream {
            return false;
        }
    }
    !secret_changed(&key.file, saved_at, saved_digest.as_deref())
}

pub fn save(
    path: &Path,
    snapshot: &HashMap<String, SavedHealth>,
    keys: &HashMap<String, Secret>,
    config: &Config,
) {
    save_with_shaper(path, snapshot, &BTreeMap::new(), keys, config);
}

pub fn save_with_shaper(
    path: &Path,
    snapshot: &HashMap<String, SavedHealth>,
    shaper: &BTreeMap<String, BucketSnapshot>,
    keys: &HashMap<String, Secret>,
    config: &Config,
) {
    let now = unix_millis(SystemTime::now());
    let entries = snapshot
        .iter()
        .map(|(name, health)| {
            // Store the upstream the key targeted when saved; absent in older
            // state files. A re-pointed key (same name and secret, different
            // upstream) must not inherit the saved health.
            let upstream = config.keys.get(name).map(|k| k.upstream.clone());
            (
                name.clone(),
                Entry {
                    disabled: health.disabled,
                    recovery: health
                        .cooling
                        .map_or(0, |cooling| now + cooling.as_millis() as u64),
                    failures: health.transient_failures,
                    key_sha256: keys
                        .get(name)
                        .map(|key| crate::auth::sha256_hex(key.expose())),
                    upstream,
                },
            )
        })
        .collect();
    let shaper = shaper
        .iter()
        .filter_map(|(name, snapshot)| {
            let upstream = config.keys.get(name)?.upstream.clone();
            Some((
                name.clone(),
                ShaperEntry {
                    learned_rpm: snapshot.learned_rpm,
                    pause_secs: snapshot.pause.map(ceil_secs),
                    closed_secs: snapshot.closed.map(ceil_secs),
                    daily: snapshot.daily.map(QuotaEntry::from),
                    monthly: snapshot.monthly.map(QuotaEntry::from),
                    key_sha256: keys
                        .get(name)
                        .map(|key| crate::auth::sha256_hex(key.expose())),
                    upstream: Some(upstream),
                },
            ))
        })
        .collect();
    let file = File {
        saved_at: now,
        keys: entries,
        shaper,
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
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ceil_secs(duration: Duration) -> u64 {
    duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
}

#[derive(Debug, Serialize, Deserialize)]
struct File {
    saved_at: u64,
    #[serde(default)]
    keys: BTreeMap<String, Entry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    shaper: BTreeMap<String, ShaperEntry>,
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
    /// Upstream the key targeted when saved; absent in older state files. A re-pointed
    /// key (same name and secret, different upstream) must not inherit the saved health.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
}

/// The shaper's durable facts for one key. Durations are rounded up to whole
/// seconds; the quota windows keep their absolute start so a later window resets.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ShaperEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    learned_rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    closed_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    daily: Option<QuotaEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    monthly: Option<QuotaEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    upstream: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct QuotaEntry {
    start: i64,
    requests: u32,
    tokens: u64,
}

impl From<QuotaSnapshot> for QuotaEntry {
    fn from(saved: QuotaSnapshot) -> Self {
        Self {
            start: saved.start,
            requests: saved.requests,
            tokens: saved.tokens,
        }
    }
}

impl ShaperEntry {
    fn into_snapshot(self) -> BucketSnapshot {
        BucketSnapshot {
            learned_rpm: self.learned_rpm.filter(|cap| *cap > 0),
            pause: self
                .pause_secs
                .filter(|secs| *secs > 0)
                .map(Duration::from_secs),
            closed: self
                .closed_secs
                .filter(|secs| *secs > 0)
                .map(Duration::from_secs),
            daily: self.daily.map(Into::into),
            monthly: self.monthly.map(Into::into),
        }
    }
}

impl From<QuotaEntry> for QuotaSnapshot {
    fn from(entry: QuotaEntry) -> Self {
        Self {
            start: entry.start,
            requests: entry.requests,
            tokens: entry.tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::{KeyState, Policy};
    use crate::classify::FailureClass;
    use std::time::Instant;

    fn config_with_keys(files: &[(&str, &str)]) -> Config {
        config_with_keys_upstream("zen", files)
    }

    fn config_with_keys_upstream(upstream: &str, files: &[(&str, &str)]) -> Config {
        let mut text =
            format!("[upstream.{upstream}]\nbase_url = \"https://opencode.ai/{upstream}/v1\"\n");
        for (name, file) in files {
            text.push_str(&format!(
                "[keys.{name}]\nupstream = \"{upstream}\"\nfile = \"{file}\"\n"
            ));
        }
        Config::parse(&text).unwrap()
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("reiki-state-{}-{tag}.toml", std::process::id()))
    }

    #[test]
    #[cfg(unix)]
    fn a_read_only_directory_fails_the_probe_with_its_path_and_cause() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("reiki-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(check_writable(&dir).is_ok());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root bypasses mode bits, so there is nothing to assert on a root runner.
        let root_bypasses = std::fs::File::create(dir.join(".root-check")).is_ok();
        if !root_bypasses {
            let err = check_writable(&dir).unwrap_err();
            assert!(err.contains(&dir.display().to_string()), "{err}");
            assert!(
                err.contains("not writable") && err.contains("Permission denied"),
                "{err}"
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_probe_never_reuses_a_planted_file_or_follows_a_symlink() {
        let dir = std::env::temp_dir().join(format!("reiki-probe-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "precious").unwrap();

        std::fs::write(dir.join("planted"), "precious").unwrap();
        let err = probe_file(&dir, "planted").unwrap_err();
        assert!(
            err.contains("not writable") && err.contains("exists"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("planted")).unwrap(),
            "precious"
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&victim, dir.join("linked")).unwrap();
            let err = probe_file(&dir, "linked").unwrap_err();
            assert!(err.contains("not writable"), "{err}");
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        }

        check_writable(&dir).unwrap();
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(left.contains(&victim) && left.contains(&dir.join("planted")));
        assert!(
            left.iter().all(|p| !p
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("reiki-probe")),
            "the probe cleaned up after itself: {left:?}"
        );
        #[cfg(unix)]
        assert!(
            dir.join("linked").symlink_metadata().is_ok(),
            "the symlink was left alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key file last modified an hour ago, so state saved now is newer than it.
    fn old_key_file(tag: &str) -> PathBuf {
        let path = temp_path(&format!("{tag}-key"));
        crate::keys::write_key(&path, "sk-fake-secret");
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
                    upstream: None, // no upstream in legacy write_state
                },
            );
        }
        let file = File {
            saved_at,
            keys,
            shaper: BTreeMap::new(),
        };
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
            &config,
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
        crate::keys::write_key(&key_file, "sk-fake-secret");
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
        crate::keys::write_key(&key_file, "sk-fake-secret");
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
        crate::keys::write_key(&key_file, "sk-fake-secret\n");
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
            &config,
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
    fn hostile_recovery_and_saved_at_values_are_rejected_or_capped() {
        let path = temp_path("hostile-recovery");
        let _ = std::fs::remove_file(&path);
        let key_file = old_key_file("hostile-recovery");
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        // The recorded digest keeps the key eligible regardless of saved_at, so the
        // test isolates how the timestamps themselves are handled.
        let digest = crate::auth::sha256_hex("sk-fake-secret");
        let now = unix_millis(SystemTime::now());

        let write = |saved_at: &str, recovery: &str| {
            std::fs::write(
                &path,
                format!(
                    "saved_at = {saved_at}\n[keys.a]\nrecovery = {recovery}\n\
                     key_sha256 = \"{digest}\"\nupstream = \"zen\"\n"
                ),
            )
            .unwrap();
        };

        // The largest TOML integer is accepted, but capped before it reaches Instant.
        write(&now.to_string(), &(i64::MAX as u64).to_string());
        let loaded = load(&path, &config);
        assert_eq!(loaded["a"].cooling, Some(MAX_RESTORED_COOLDOWN));
        let mut state = KeyState::default();
        state.restore("a", loaded["a"], Instant::now());
        assert!(state.cooling_remaining("a", Instant::now()).is_some());

        // A value beyond TOML's integer range is rejected with the whole file.
        write(&now.to_string(), "18446744073709551615");
        assert!(load(&path, &config).is_empty());

        // Zero and a deadline that has already passed never become a cooldown.
        write(&now.to_string(), "0");
        assert_eq!(load(&path, &config)["a"].cooling, None);
        write(&now.to_string(), &now.to_string());
        assert_eq!(load(&path, &config)["a"].cooling, None);

        // Negative, NaN-like and float values are not u64: the file is ignored,
        // rather than reaching any Instant/SystemTime arithmetic.
        for recovery in ["-1", "nan", "inf", "-inf", "1e30", "\"NaN\""] {
            write(&now.to_string(), recovery);
            assert!(load(&path, &config).is_empty(), "recovery = {recovery}");
        }
        for saved_at in ["-1", "nan", "inf", "1e30"] {
            write(saved_at, &(now + 3_600_000).to_string());
            assert!(load(&path, &config).is_empty(), "saved_at = {saved_at}");
        }

        // A huge but valid saved_at converts to SystemTime without panicking.
        write(
            &(i64::MAX as u64).to_string(),
            &(now + 3_600_000).to_string(),
        );
        assert!(load(&path, &config)["a"].cooling.is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
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
            &config,
        );
        assert!(load(&path, &config)["a"].disabled, "same file is restored");

        crate::keys::write_key(&key_file, "a-longer-replacement-secret");
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
    fn a_repointed_key_starts_healthy() {
        let path = temp_path("repointed");
        let _ = std::fs::remove_file(&path);
        let key_file = old_key_file("repointed");
        let zen = config_with_keys_upstream("zen", &[("a", key_file.to_str().unwrap())]);
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
            &crate::proxy::load_keys(&zen).unwrap(),
            &zen,
        );
        assert!(load(&path, &zen)["a"].disabled, "same upstream is restored");

        // Same name, same secret, different upstream: the saved health must not follow.
        let other = config_with_keys_upstream("other", &[("a", key_file.to_str().unwrap())]);
        assert!(load(&path, &other).is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn the_same_upstream_restores_saved_health() {
        let path = temp_path("same-upstream");
        let _ = std::fs::remove_file(&path);
        let key_file = old_key_file("same-upstream");
        let config = config_with_keys_upstream("zen", &[("a", key_file.to_str().unwrap())]);
        let mut state = KeyState::default();
        state.record_failure(
            "a",
            FailureClass::RateLimited(Some(Duration::from_secs(50))),
            &Policy::default(),
            Instant::now(),
        );
        save(
            &path,
            &state.snapshot(Instant::now()),
            &crate::proxy::load_keys(&config).unwrap(),
            &config,
        );
        let loaded = load(&path, &config);
        assert!(loaded["a"].cooling.unwrap() > Duration::from_secs(40));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn older_state_without_the_upstream_field_still_restores() {
        let path = temp_path("legacy");
        let key_file = old_key_file("legacy");
        let config = config_with_keys_upstream("zen", &[("a", key_file.to_str().unwrap())]);
        let now = unix_millis(SystemTime::now());
        write_state(&path, now, &[("a", false, now + 3_600_000, 2)]);
        let loaded = load(&path, &config);
        assert_eq!(loaded.len(), 1);
        assert!(loaded["a"].cooling.is_some());
        assert_eq!(loaded["a"].transient_failures, 2);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn a_shaper_snapshot_round_trips_through_the_file() {
        let path = temp_path("shaper-roundtrip");
        let _ = std::fs::remove_file(&path);
        let key_file = old_key_file("shaper-roundtrip");
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        let shaper = BTreeMap::from([(
            "a".to_owned(),
            BucketSnapshot {
                learned_rpm: Some(3),
                pause: Some(Duration::from_secs(30)),
                closed: Some(Duration::from_secs(60)),
                daily: Some(QuotaSnapshot {
                    start: 1_700_000_000,
                    requests: 2,
                    tokens: 100,
                }),
                monthly: None,
            },
        )]);
        save_with_shaper(
            &path,
            &HashMap::new(),
            &shaper,
            &crate::proxy::load_keys(&config).unwrap(),
            &config,
        );

        assert_eq!(load_shaper(&path, &config), shaper);
        assert_eq!(load_state(&path, &config).shaper, shaper);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn a_rotated_key_does_not_restore_shaper_state() {
        let path = temp_path("shaper-rotated");
        let _ = std::fs::remove_file(&path);
        let key_file = old_key_file("shaper-rotated");
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        let shaper = BTreeMap::from([(
            "a".to_owned(),
            BucketSnapshot {
                learned_rpm: Some(3),
                ..BucketSnapshot::default()
            },
        )]);
        let keys = crate::proxy::load_keys(&config).unwrap();
        save_with_shaper(&path, &HashMap::new(), &shaper, &keys, &config);
        assert_eq!(load_shaper(&path, &config).len(), 1);

        crate::keys::write_key(&key_file, "a-different-secret");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&key_file)
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        assert!(load_shaper(&path, &config).is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn a_repointed_upstream_does_not_restore_shaper_state() {
        let path = temp_path("shaper-repointed");
        let _ = std::fs::remove_file(&path);
        let key_file = old_key_file("shaper-repointed");
        let zen = config_with_keys_upstream("zen", &[("a", key_file.to_str().unwrap())]);
        let shaper = BTreeMap::from([(
            "a".to_owned(),
            BucketSnapshot {
                learned_rpm: Some(3),
                ..BucketSnapshot::default()
            },
        )]);
        save_with_shaper(
            &path,
            &HashMap::new(),
            &shaper,
            &crate::proxy::load_keys(&zen).unwrap(),
            &zen,
        );
        assert_eq!(load_shaper(&path, &zen), shaper);

        let other = config_with_keys_upstream("other", &[("a", key_file.to_str().unwrap())]);
        assert!(load_shaper(&path, &other).is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&key_file);
    }

    #[test]
    fn an_empty_shaper_table_serialises_like_the_old_file() {
        #[derive(serde::Serialize)]
        struct Legacy {
            saved_at: u64,
            keys: BTreeMap<String, Entry>,
        }

        let file = File {
            saved_at: 1,
            keys: BTreeMap::new(),
            shaper: BTreeMap::new(),
        };
        let legacy = Legacy {
            saved_at: 1,
            keys: BTreeMap::new(),
        };
        let text = toml::to_string(&file).unwrap();
        assert_eq!(text, toml::to_string(&legacy).unwrap());
        assert!(!text.contains("shaper"));
    }

    #[test]
    fn older_state_without_a_shaper_table_loads_without_one() {
        let path = temp_path("no-shaper");
        let key_file = old_key_file("no-shaper");
        let config = config_with_keys(&[("a", key_file.to_str().unwrap())]);
        let now = unix_millis(SystemTime::now());
        write_state(&path, now, &[("a", false, now + 3_600_000, 2)]);
        assert_eq!(load(&path, &config).len(), 1);
        assert!(load_shaper(&path, &config).is_empty());
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
