//! A bounded in-memory ring of recent key failures, and the `reiki failures` table.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::classify::civil_from_days;

/// Records kept across all streams. Small on purpose: this is a live diagnostic, not a log.
pub const CAPACITY: usize = 64;
/// Longest body snippet kept, matching the existing failure log line.
pub const MAX_NOTE_CHARS: usize = 200;
/// Longest `Retry-After` kept. The value comes from the upstream and is only echoed.
pub const MAX_RETRY_AFTER_CHARS: usize = 64;

/// One key failure as `/v1/failures` and `reiki failures` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// Unix seconds, from the wall clock the instance uses elsewhere.
    pub time: u64,
    pub stream: String,
    pub key: String,
    pub status: Option<u16>,
    pub class: String,
    /// As received, bounded and printable; never parsed.
    pub retry_after: Option<String>,
    /// Whether this failure put the key on cooldown.
    pub cooled: bool,
    /// Seconds left on the key's cooldown, when it has one.
    pub cooling_seconds: Option<u64>,
    /// Scrubbed, printable body snippet.
    pub note: String,
}

/// The last [`CAPACITY`] failures, newest last.
#[derive(Debug)]
pub struct FailureRing {
    records: Mutex<VecDeque<FailureRecord>>,
}

impl Default for FailureRing {
    fn default() -> Self {
        Self::new()
    }
}

impl FailureRing {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        }
    }

    /// Adds one record. Contention drops it: an observability buffer must never make a request
    /// wait, and the ring is a sample anyway.
    pub fn record(&self, record: FailureRecord) {
        let Ok(mut records) = self.records.try_lock() else {
            return;
        };
        if records.len() == CAPACITY {
            records.pop_front();
        }
        records.push_back(record);
    }

    /// Newest first, at most `limit`, optionally only failures for one key.
    pub fn snapshot(&self, limit: usize, key: Option<&str>) -> Vec<FailureRecord> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records
            .iter()
            .rev()
            .filter(|record| key.is_none_or(|key| record.key == key))
            .take(limit)
            .cloned()
            .collect()
    }

    /// How many records each failure class has in the ring; only observed classes.
    pub fn counts(&self) -> BTreeMap<String, u64> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = BTreeMap::new();
        for record in records.iter() {
            *out.entry(record.class.clone()).or_insert(0) += 1;
        }
        out
    }
}

/// Turns a `/v1/failures` answer into rows.
pub fn parse(body: &str) -> Result<Vec<FailureRecord>, String> {
    serde_json::from_str(body).map_err(|e| format!("cannot read /v1/failures: {e}"))
}

/// The JSON document `reiki failures --json` prints: the same array as the endpoint.
pub fn render_json(records: &[FailureRecord]) -> Result<String, String> {
    serde_json::to_string(records)
        .map(|text| format!("{text}\n"))
        .map_err(|e| format!("cannot write JSON: {e}"))
}

/// The table `reiki failures` prints, clipped to `width` characters per line.
pub fn render_table(records: &[FailureRecord], width: usize) -> String {
    if records.is_empty() {
        return "no failures\n".to_owned();
    }
    let headers = ["TIME", "STREAM", "KEY", "STATUS", "CLASS", "COOLED", "NOTE"];
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|record| {
            vec![
                format_time(record.time),
                record.stream.clone(),
                record.key.clone(),
                record
                    .status
                    .map_or_else(|| "-".to_owned(), |status| status.to_string()),
                record.class.clone(),
                match (record.cooled, record.cooling_seconds) {
                    (true, Some(secs)) => format!("{secs}s"),
                    (true, None) => "yes".to_owned(),
                    (false, _) => "-".to_owned(),
                },
                record.note.clone(),
            ]
        })
        .collect();
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    // Give the note whatever is left, so the fixed columns keep their shape.
    let fixed: usize = widths[..headers.len() - 1].iter().sum::<usize>() + 2 * (headers.len() - 1);
    widths[headers.len() - 1] = widths[headers.len() - 1]
        .min(width.saturating_sub(fixed))
        .max(headers[headers.len() - 1].chars().count().min(width));
    let mut out = String::new();
    let header_row: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    write_row(&mut out, &header_row, &widths, width);
    for row in &rows {
        write_row(&mut out, row, &widths, width);
    }
    out
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize], width: usize) {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        if index + 1 == cells.len() {
            line.push_str(&clip(cell, widths[index]));
        } else {
            let text = clip(cell, widths[index]);
            line.push_str(&text);
            line.push_str(&" ".repeat(widths[index].saturating_sub(text.chars().count())));
        }
    }
    out.push_str(&clip(&line, width));
    out.push('\n');
}

/// Truncates to `max` characters without splitting a character.
fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Unix seconds as `YYYY-MM-DD HH:MM:SS` in UTC, with no time-zone dependency.
fn format_time(unix: u64) -> String {
    let secs = i64::try_from(unix).unwrap_or(i64::MAX);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(label: &str) -> FailureRecord {
        FailureRecord {
            time: 1,
            stream: "s".to_owned(),
            key: label.to_owned(),
            status: Some(429),
            class: "rate_limited".to_owned(),
            retry_after: Some("120".to_owned()),
            cooled: true,
            cooling_seconds: Some(10),
            note: format!("failure {label}"),
        }
    }

    #[test]
    fn the_ring_keeps_the_newest_capacity_in_order() {
        let ring = FailureRing::new();
        for n in 0..CAPACITY + 5 {
            ring.record(failure(&n.to_string()));
        }
        let all = ring.snapshot(CAPACITY, None);
        assert_eq!(all.len(), CAPACITY);
        assert_eq!(all.first().unwrap().key, (CAPACITY + 4).to_string());
        assert_eq!(all.last().unwrap().key, "5");
    }

    #[test]
    fn snapshot_is_newest_first_and_honours_limit_and_key() {
        let ring = FailureRing::new();
        ring.record(failure("a"));
        ring.record(failure("b"));
        ring.record(failure("a"));
        let all = ring.snapshot(10, None);
        let keys: Vec<&str> = all.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["a", "b", "a"]);
        assert_eq!(ring.snapshot(1, None)[0].key, "a");
        let only = ring.snapshot(10, Some("a"));
        assert_eq!(only.len(), 2);
        assert!(only.iter().all(|r| r.key == "a"));
        assert!(ring.snapshot(10, Some("gone")).is_empty());
    }

    #[test]
    fn counts_name_only_the_classes_present() {
        let ring = FailureRing::new();
        ring.record(failure("a"));
        let mut other = failure("b");
        other.class = "transient".to_owned();
        ring.record(other);
        let counts = ring.counts();
        assert_eq!(counts.get("rate_limited"), Some(&1));
        assert_eq!(counts.get("transient"), Some(&1));
        assert_eq!(counts.get("quota_exhausted"), None);
    }

    #[test]
    fn the_table_has_the_columns_and_fits_the_width() {
        let text = render_table(&[failure("a")], 80);
        let header = text.lines().next().unwrap();
        for column in ["TIME", "STREAM", "KEY", "STATUS", "CLASS", "COOLED", "NOTE"] {
            assert!(header.contains(column), "{header}");
        }
        assert!(text.contains("429"), "{text}");
        assert!(text.contains("10s"), "{text}");
        for line in render_table(&[failure("long-key")], 24).lines() {
            assert!(line.chars().count() <= 24, "{line:?}");
        }
    }

    #[test]
    fn an_empty_ring_says_so() {
        assert_eq!(render_table(&[], 80), "no failures\n");
    }

    #[test]
    fn time_is_utc_without_a_time_zone_dependency() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
        assert_eq!(format_time(1_700_000_000), "2023-11-14 22:13:20");
    }
}
