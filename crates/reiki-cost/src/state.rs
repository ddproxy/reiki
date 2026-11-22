//! Bounded, durable cost counters. The layout mirrors Reiki's own state files: a sibling
//! TOML file, mode 0600, written to a unique temporary file and renamed into place.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use reiki_plugin::{civil_from_days, days_from_civil};
use serde::{Deserialize, Serialize};

pub const MAX_BUCKETS: usize = 4096;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
static STAMP: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    Hour,
    Day,
    Month,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bucket {
    pub granularity: Granularity,
    pub start: i64,
    pub currency: String,
    pub stream: String,
    pub upstream: String,
    pub model: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(serialize_with = "as_string", deserialize_with = "from_string")]
    pub micros: u128,
    pub unpriced: u64,
}

fn as_string<T: std::fmt::Display, S: serde::Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

fn from_string<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    version: u8,
    buckets: Vec<Bucket>,
}

#[derive(Debug)]
pub struct State {
    path: Option<PathBuf>,
    buckets: Vec<Bucket>,
    /// False after an unreadable or unwritable state file. Block budgets fail closed on this.
    pub healthy: bool,
}

impl State {
    pub fn load(path: Option<PathBuf>, now: SystemTime) -> Self {
        let Some(path) = path else {
            return Self {
                path: None,
                buckets: Vec::new(),
                healthy: true,
            };
        };
        let loaded = std::fs::metadata(&path)
            .ok()
            .filter(|meta| meta.is_file() && meta.len() <= MAX_FILE_BYTES)
            .and_then(|_| std::fs::read_to_string(&path).ok())
            .and_then(|text| toml::from_str::<StateFile>(&text).ok())
            .filter(|file| file.version == 1);
        let was_readable = loaded.is_some();
        let mut state = Self {
            path: Some(path),
            buckets: loaded.map(|file| file.buckets).unwrap_or_default(),
            healthy: true,
        };
        state.healthy = was_readable || !std::fs::metadata(state.path.as_ref().unwrap()).is_ok();
        state.buckets.retain(|bucket| valid_bucket(bucket, now));
        state.prune(now);
        state
    }

    /// Replaces the plugin's path after `configure` supplies the config location.
    pub fn set_path(&mut self, path: PathBuf, now: SystemTime) {
        *self = Self::load(Some(path), now);
    }

    pub fn buckets(&self) -> &[Bucket] {
        &self.buckets
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        now: SystemTime,
        currency: &str,
        stream: &str,
        upstream: &str,
        model: &str,
        micros: u128,
        input_tokens: u64,
        output_tokens: u64,
        unpriced: bool,
    ) {
        let seconds = unix_seconds(now);
        for granularity in [Granularity::Hour, Granularity::Day, Granularity::Month] {
            let start = period_start(seconds, granularity);
            let bucket = self.buckets.iter_mut().find(|bucket| {
                bucket.granularity == granularity
                    && bucket.start == start
                    && bucket.currency == currency
                    && bucket.stream == stream
                    && bucket.upstream == upstream
                    && bucket.model == model
            });
            if let Some(bucket) = bucket {
                bucket.requests = bucket.requests.saturating_add(1);
                bucket.input_tokens = bucket.input_tokens.saturating_add(input_tokens);
                bucket.output_tokens = bucket.output_tokens.saturating_add(output_tokens);
                bucket.micros = bucket.micros.saturating_add(micros);
                bucket.unpriced = bucket.unpriced.saturating_add(u64::from(unpriced));
            } else {
                self.buckets.push(Bucket {
                    granularity,
                    start,
                    currency: currency.to_owned(),
                    stream: stream.to_owned(),
                    upstream: upstream.to_owned(),
                    model: model.to_owned(),
                    requests: 1,
                    input_tokens,
                    output_tokens,
                    micros,
                    unpriced: u64::from(unpriced),
                });
            }
        }
        self.prune(now);
    }

    /// Spend in one current UTC period, before a request is admitted.
    pub fn current_spend(&self, stream: &str, granularity: Granularity, now: SystemTime) -> u128 {
        let start = period_start(unix_seconds(now), granularity);
        self.buckets
            .iter()
            .filter(|bucket| {
                bucket.stream == stream
                    && bucket.granularity == granularity
                    && bucket.start == start
            })
            .fold(0u128, |total, bucket| total.saturating_add(bucket.micros))
    }

    /// Persists the current counters. A failure leaves the in-memory figures intact but
    /// marks the state unhealthy so block budgets can fail closed.
    pub fn save(&mut self, now: SystemTime) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.prune(now);
        let file = StateFile {
            version: 1,
            buckets: self.buckets.clone(),
        };
        let text = match toml::to_string(&file) {
            Ok(text) => text,
            Err(_) => {
                self.healthy = false;
                return;
            }
        };
        if write_atomically(&path, &text).is_err() {
            self.healthy = false;
        }
    }

    fn prune(&mut self, now: SystemTime) {
        let seconds = unix_seconds(now);
        self.buckets.retain(|bucket| match bucket.granularity {
            Granularity::Hour => bucket.start > seconds - 7 * DAY,
            Granularity::Day => bucket.start > seconds - 62 * DAY,
            Granularity::Month => bucket.start > seconds - 25 * 31 * DAY,
        });
        if self.buckets.len() > MAX_BUCKETS {
            self.buckets
                .sort_by_key(|bucket| std::cmp::Reverse(bucket.start));
            self.buckets.truncate(MAX_BUCKETS);
        }
    }
}

fn valid_bucket(bucket: &Bucket, now: SystemTime) -> bool {
    let now = unix_seconds(now);
    bucket.start <= now
        && bucket.start > now - 25 * 31 * DAY
        && bucket.currency.len() <= 16
        && bucket.stream.len() <= 128
        && bucket.upstream.len() <= 128
        && bucket.model.len() <= 128
        && bucket.requests <= 1_000_000_000
}

#[cfg(test)]
pub(crate) fn now_for_test(stamp: &str) -> SystemTime {
    let year: i64 = stamp[..4].parse().unwrap();
    let month: i64 = stamp[5..7].parse().unwrap();
    let day: i64 = stamp[8..10].parse().unwrap();
    let hour: i64 = stamp[11..13].parse().unwrap();
    let minute: i64 = stamp[14..16].parse().unwrap();
    let seconds =
        days_from_civil(year, month, 1) * DAY + (day - 1) * DAY + hour * HOUR + minute * 60;
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64)
}

pub fn unix_seconds(now: SystemTime) -> i64 {
    now.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn period_start(seconds: i64, granularity: Granularity) -> i64 {
    let days = seconds.div_euclid(DAY);
    match granularity {
        Granularity::Hour => seconds.div_euclid(HOUR) * HOUR,
        Granularity::Day => days * DAY,
        Granularity::Month => {
            let (year, month, _) = civil_from_days(days);
            days_from_civil(year, month, 1) * DAY
        }
    }
}

/// UTC period key `YYYY-MM`, `YYYY-MM-DD` or `YYYY-MM-DDTHH`, all derived from a Unix stamp.
pub fn period_key(seconds: i64, granularity: Granularity) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(DAY));
    let hour = seconds.rem_euclid(DAY) / HOUR;
    match granularity {
        Granularity::Month => format!("{year:04}-{month:02}"),
        Granularity::Day => format!("{year:04}-{month:02}-{day:02}"),
        Granularity::Hour => format!("{year:04}-{month:02}-{day:02}T{hour:02}"),
    }
}

/// Parses a requested period filter and returns its granularity, start and end bounds.
pub fn parse_period(text: &str) -> Option<(Granularity, i64, i64)> {
    let digits = |part: &str, min: i64, max: i64| -> Option<i64> {
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value = part.parse::<i64>().ok()?;
        (value >= min && value <= max).then_some(value)
    };
    let month = |year: &str, mon: &str| -> Option<(i64, i64)> {
        let year = if year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit()) {
            year.parse::<i64>().ok()?
        } else {
            return None;
        };
        let month = digits(mon, 1, 12)?;
        let next = if month == 12 {
            days_from_civil(year + 1, 1, 1)
        } else {
            days_from_civil(year, month + 1, 1)
        };
        Some((days_from_civil(year, month, 1) * DAY, next * DAY))
    };
    match text.len() {
        7 => {
            let (year, mon) = text.split_once('-')?;
            let (start, end) = month(year, mon)?;
            Some((Granularity::Month, start, end))
        }
        10 => {
            let (date, day) = text.rsplit_once('-')?;
            let (year, mon) = date.split_once('-')?;
            let (month_start, _) = month(year, mon)?;
            let start = month_start + (digits(day, 1, 31)? - 1) * DAY;
            Some((Granularity::Day, start, start + DAY))
        }
        13 => {
            let (date, hour) = text.split_once('T')?;
            let (year, mon) = date[..7].split_once('-')?;
            let day = digits(&date[8..], 1, 31)?;
            let (month_start, _) = month(year, mon)?;
            let start = month_start + (day - 1) * DAY + digits(hour, 0, 23)? * HOUR;
            Some((Granularity::Hour, start, start + HOUR))
        }
        _ => None,
    }
}

/// Writes mode 0600 to a fresh sibling file, fsyncs, then renames into place.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("state path has no file name"))?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        STAMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_keys_and_bounds_roll_over() {
        let seconds = days_from_civil(2026, 1, 1) * DAY + 12 * HOUR;
        assert_eq!(period_key(seconds, Granularity::Hour), "2026-01-01T12");
        assert_eq!(period_key(seconds, Granularity::Day), "2026-01-01");
        assert_eq!(period_key(seconds, Granularity::Month), "2026-01");
        let (granularity, start, end) = parse_period("2026-11").unwrap();
        assert_eq!(granularity, Granularity::Month);
        assert_eq!(start, days_from_civil(2026, 11, 1) * DAY);
        assert_eq!(end, days_from_civil(2026, 12, 1) * DAY);
        assert!(parse_period("2026-13").is_none());
        assert!(parse_period("2026-11-31T99").is_none());
    }

    #[test]
    fn monthly_spend_rolls_over_at_utc_midnight() {
        let mut state = State::load(None, now_for("2026-11-30T23:30"));
        state.record(
            now_for("2026-11-30T23:30"),
            "USD",
            "one",
            "main",
            "m",
            100,
            10,
            5,
            false,
        );
        assert_eq!(
            state.current_spend("one", Granularity::Month, now_for("2026-11-30T23:30")),
            100
        );
        let after = now_for("2026-12-01T00:30");
        assert_eq!(state.current_spend("one", Granularity::Month, after), 0);
        assert_eq!(state.current_spend("one", Granularity::Day, after), 0);
    }

    #[test]
    fn state_file_is_bounded_mode_0600_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!("reiki-cost-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reiki.cost.state");
        let mut state = State::load(Some(path.clone()), now_for("2026-11-02T03:00"));
        state.record(
            now_for("2026-11-02T03:00"),
            "USD",
            "one",
            "main",
            "m",
            7,
            1,
            2,
            false,
        );
        state.save(now_for("2026-11-02T03:00"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let reloaded = State::load(Some(path.clone()), now_for("2026-11-02T03:00"));
        assert!(reloaded.healthy);
        assert_eq!(reloaded.buckets().len(), 3);
        assert_eq!(
            reloaded.current_spend("one", Granularity::Day, now_for("2026-11-02T03:00")),
            7
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_state_fails_closed() {
        let dir = std::env::temp_dir().join(format!("reiki-cost-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reiki.cost.state");
        std::fs::write(&path, b"not toml").unwrap();
        let state = State::load(Some(path), now_for("2026-11-02T03:00"));
        assert!(!state.healthy);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn now_for(stamp: &str) -> SystemTime {
        // Stamps used in tests are all UTC and have the shape YYYY-MM-DDTHH:MM.
        let year: i64 = stamp[..4].parse().unwrap();
        let month: i64 = stamp[5..7].parse().unwrap();
        let day: i64 = stamp[8..10].parse().unwrap();
        let hour: i64 = stamp[11..13].parse().unwrap();
        let minute: i64 = stamp[14..16].parse().unwrap();
        let seconds =
            days_from_civil(year, month, 1) * DAY + (day - 1) * DAY + hour * HOUR + minute * 60;
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64)
    }

    #[test]
    fn rounding_helper_is_half_up() {
        assert_eq!(crate::money::round_div(5, 2), 3);
        assert_eq!(crate::money::round_div(4, 2), 2);
    }
}
