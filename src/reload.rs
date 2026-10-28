//! What a config reload did, and the record of the last ones for `/status`.

use std::time::SystemTime;

use crate::proxy::json_escape;

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
}

impl ReloadReport {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"ok\":true,\"generation\":{},\"keys\":{},\"streams\":{},\"changed\":{},\"restart_needed\":{}}}",
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
        }
    }

    #[test]
    fn the_report_names_what_changed_and_what_needs_a_restart() {
        let r = report(4);
        assert_eq!(
            r.to_json(),
            "{\"ok\":true,\"generation\":4,\"keys\":3,\"streams\":2,\"changed\":[\"plugins phi\",\"tunnel allow +2/-0\"],\"restart_needed\":[\"listen\"]}"
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
    fn timestamps_are_rfc3339_in_utc() {
        assert_eq!(rfc3339(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_229_850);
        assert_eq!(rfc3339(t), "2026-10-28T23:24:10Z");
    }
}
