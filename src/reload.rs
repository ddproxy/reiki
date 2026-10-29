//! What a config reload did, and the record of the last ones for `/status`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::plugins::{stamp, FileStamp};
use crate::proxy::{json_escape, App};

/// The outcome of a successful reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// How many reloads have succeeded since startup, this one included.
    pub generation: u64,
    /// What differs from before, such as `plugins phi` or `tunnel allow +2/-0`.
    pub changed: Vec<String>,
    /// Settings that differ in the file but only apply after a restart.
    pub restart_needed: Vec<String>,
    pub keys: usize,
    pub streams: usize,
    /// True for a preview: the report says what a reload would do, and nothing was applied.
    pub dry_run: bool,
}

impl ReloadReport {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"ok\":true,\"dry_run\":{},\"generation\":{},\"keys\":{},\"streams\":{},\"changed\":{},\"restart_needed\":{}}}",
            self.dry_run,
            self.generation,
            self.keys,
            self.streams,
            json_list(&self.changed),
            json_list(&self.restart_needed)
        )
    }

    /// One log line, with no secrets: names and counts only.
    pub fn summary(&self) -> String {
        let changed = if self.changed.is_empty() {
            "no setting changed".to_owned()
        } else {
            self.changed.join(", ")
        };
        format!(
            "{changed}; keys={} streams={}; restart needed=[{}]",
            self.keys,
            self.streams,
            self.restart_needed.join(", ")
        )
    }
}

/// The JSON body for a failed reload.
pub fn error_json(error: &str) -> String {
    format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(error))
}

fn json_list(items: &[String]) -> String {
    let inner = items
        .iter()
        .map(|i| format!("\"{}\"", json_escape(i)))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

struct Last {
    at: SystemTime,
    error: Option<String>,
}

/// Counts successful reloads and remembers the last attempt, for `/status`.
pub struct ReloadState {
    generation: u64,
    loaded_at: SystemTime,
    last: Option<Last>,
}

impl ReloadState {
    pub fn new() -> Self {
        Self {
            generation: 0,
            loaded_at: SystemTime::now(),
            last: None,
        }
    }

    /// The generation the next successful reload will have.
    pub fn next_generation(&self) -> u64 {
        self.generation + 1
    }

    pub fn record_ok(&mut self, report: &ReloadReport) {
        self.generation = report.generation;
        self.loaded_at = SystemTime::now();
        self.last = Some(Last {
            at: self.loaded_at,
            error: None,
        });
    }

    pub fn record_error(&mut self, error: &str) {
        self.last = Some(Last {
            at: SystemTime::now(),
            error: Some(error.to_owned()),
        });
    }

    /// The `config` object of `/status`.
    pub fn status_json(&self) -> String {
        let last = match &self.last {
            None => "null".to_owned(),
            Some(last) => match &last.error {
                None => format!("{{\"ok\":true,\"at\":\"{}\"}}", rfc3339(last.at)),
                Some(error) => format!(
                    "{{\"ok\":false,\"at\":\"{}\",\"error\":\"{}\"}}",
                    rfc3339(last.at),
                    json_escape(error)
                ),
            },
        };
        format!(
            "{{\"generation\":{},\"loaded_at\":\"{}\",\"last_reload\":{last}}}",
            self.generation,
            rfc3339(self.loaded_at)
        )
    }
}

impl Default for ReloadState {
    fn default() -> Self {
        Self::new()
    }
}

type Stamps = Vec<(PathBuf, Option<FileStamp>)>;

fn stamps(paths: &[PathBuf]) -> Stamps {
    paths.iter().map(|p| (p.clone(), stamp(p))).collect()
}

/// Notices when a set of files changed and then held still, so a half-written file is not read.
pub struct ChangeDetector {
    settled: Stamps,
    pending: Option<Stamps>,
}

impl ChangeDetector {
    /// Takes the files as they are now as the baseline.
    pub fn new(paths: &[PathBuf]) -> Self {
        Self {
            settled: stamps(paths),
            pending: None,
        }
    }

    /// Call once per interval. True when the files differ from the baseline and are the same as on
    /// the previous call, which makes them the new baseline.
    pub fn changed(&mut self, paths: &[PathBuf]) -> bool {
        let now = stamps(paths);
        if now == self.settled {
            self.pending = None;
            return false;
        }
        if self.pending.as_ref() == Some(&now) {
            self.settled = now;
            self.pending = None;
            return true;
        }
        self.pending = Some(now);
        false
    }
}

/// Reloads the config when `[reload] watch = true` and its files change. Runs for the life of the
/// process and reads the setting each round, so a reload can switch it on or off.
pub async fn watch(app: Arc<App>) {
    let mut detector = ChangeDetector::new(&app.reload_inputs());
    loop {
        let (enabled, seconds) = {
            let snapshot = app.snapshot();
            (
                snapshot.config.reload.watch,
                snapshot.config.reload.interval_secs,
            )
        };
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        if !enabled {
            detector = ChangeDetector::new(&app.reload_inputs());
            continue;
        }
        if detector.changed(&app.reload_inputs()) {
            tracing::info!("config or key files changed; reloading");
            let worker = Arc::clone(&app);
            // Stamp the files before reloading, so an edit that lands mid-reload is seen next tick.
            let baseline = ChangeDetector::new(&app.reload_inputs());
            // `reload_now` logs the outcome; a bad config is not retried until the files change again.
            let _ = tokio::task::spawn_blocking(move || worker.reload_now()).await;
            detector = baseline;
        }
    }
}

/// `2026-10-23T21:04:10Z` for a point in time, without a date library.
fn rfc3339(time: SystemTime) -> String {
    let secs = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs = i64::try_from(secs).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

// Days since 1970-01-01 to a proleptic Gregorian date (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn report(generation: u64) -> ReloadReport {
        ReloadReport {
            generation,
            changed: vec!["plugins phi".into(), "tunnel allow +2/-0".into()],
            restart_needed: vec!["listen".into()],
            keys: 3,
            streams: 2,
            dry_run: false,
        }
    }

    #[test]
    fn the_report_names_what_changed_and_what_needs_a_restart() {
        let r = report(4);
        assert_eq!(
            r.to_json(),
            "{\"ok\":true,\"dry_run\":false,\"generation\":4,\"keys\":3,\"streams\":2,\"changed\":[\"plugins phi\",\"tunnel allow +2/-0\"],\"restart_needed\":[\"listen\"]}"
        );
        assert_eq!(
            r.summary(),
            "plugins phi, tunnel allow +2/-0; keys=3 streams=2; restart needed=[listen]"
        );
        let quiet = ReloadReport {
            changed: vec![],
            restart_needed: vec![],
            ..report(1)
        };
        assert!(quiet.summary().starts_with("no setting changed;"));
    }

    #[test]
    fn the_state_counts_successes_and_remembers_the_last_failure() {
        let mut state = ReloadState::new();
        assert!(state.status_json().contains("\"generation\":0"));
        assert!(state.status_json().contains("\"last_reload\":null"));
        assert_eq!(state.next_generation(), 1);
        state.record_ok(&report(1));
        assert!(state.status_json().contains("\"generation\":1"));
        assert!(state.status_json().contains("\"ok\":true"));
        state.record_error("config: bad \"thing\"");
        let json = state.status_json();
        assert!(
            json.contains("\"generation\":1"),
            "a failure does not count: {json}"
        );
        assert!(
            json.contains("\"ok\":false") && json.contains("bad \\\"thing\\\""),
            "{json}"
        );
    }

    #[test]
    fn a_change_is_reported_once_the_files_hold_still() {
        let file = std::env::temp_dir().join(format!("reiki-detect-{}.txt", std::process::id()));
        std::fs::write(&file, "one").unwrap();
        let paths = [file.clone()];
        let mut detector = ChangeDetector::new(&paths);
        assert!(!detector.changed(&paths), "nothing changed");

        std::fs::write(&file, "one, then a longer edit").unwrap();
        assert!(
            !detector.changed(&paths),
            "first sight of a change waits a tick"
        );
        std::fs::write(&file, "still being written, now longer again").unwrap();
        assert!(
            !detector.changed(&paths),
            "a file still changing keeps waiting"
        );
        assert!(detector.changed(&paths), "unchanged for a tick: report it");
        assert!(!detector.changed(&paths), "and report it only once");

        std::fs::remove_file(&file).unwrap();
        assert!(
            !detector.changed(&paths) && detector.changed(&paths),
            "a deleted file counts"
        );
    }

    #[test]
    fn timestamps_are_rfc3339_in_utc() {
        assert_eq!(rfc3339(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_229_850);
        assert_eq!(rfc3339(t), "2026-10-28T23:24:10Z");
    }
}
