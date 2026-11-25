//! A bounded in-memory ring of recent key failures, and the `reiki failures` table.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use reiki_plugin::format_utc;

/// Default number of records kept across all streams. Overridden by `failure_ring_size`.
pub const DEFAULT_CAPACITY: usize = 200;
/// Hard ceiling for `failure_ring_size`, so a config cannot reserve unbounded memory.
pub const MAX_CAPACITY: usize = 10_000;
/// Largest `limit` accepted by `/v1/failures` and `reiki failures`. Independent of capacity.
pub const MAX_QUERY_LIMIT: usize = 64;
/// Longest body snippet kept, in bytes rather than characters.
pub const MAX_NOTE_BYTES: usize = 200;
/// Longest `Retry-After` kept, in bytes. The value comes from the upstream and is only echoed.
pub const MAX_RETRY_AFTER_BYTES: usize = 64;

/// The only class labels `/v1/failures` counts. Keeping this fixed means a future or corrupted
/// label cannot grow the peer health document into an unbounded map.
pub const CLASSES: &[&str] = &[
    "rate_limited",
    "quota_exhausted",
    "access_disabled",
    "unauthorized",
    "transient",
    "request_error",
];

pub fn is_known_class(label: &str) -> bool {
    CLASSES.contains(&label)
}

/// What the failover walk did after this failure. The names are the wire labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureAction {
    /// The key or one of its scopes was cooled.
    Cooldown,
    /// The key was taken out of rotation and needs a new secret.
    Rotate,
    /// The key was skipped for this request and the upstream answer was passed on.
    #[default]
    PassThrough,
}

impl FailureAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cooldown => "cooldown",
            Self::Rotate => "rotate",
            Self::PassThrough => "pass-through",
        }
    }
}

/// One key failure as `/v1/failures` and `reiki failures` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// RFC3339 UTC instant from the wall clock the instance uses elsewhere.
    #[serde(
        rename = "time_at",
        serialize_with = "write_time_at",
        deserialize_with = "read_time_at"
    )]
    pub time: u64,
    pub stream: String,
    pub key: String,
    pub status: Option<u16>,
    pub class: String,
    /// The action chosen after this failure; absent in records from older instances.
    #[serde(default)]
    pub action: FailureAction,
    /// As received, bounded and printable; never parsed.
    pub retry_after: Option<String>,
    /// Whether this failure put the key on cooldown.
    pub cooled: bool,
    /// Seconds left on the key's cooldown, when it has one.
    pub cooldown_seconds: Option<u64>,
    /// Scrubbed, printable body snippet. `None` for read-only admin callers, who get no free text.
    pub note: Option<String>,
}

#[derive(Debug)]
struct State {
    capacity: usize,
    records: VecDeque<FailureRecord>,
}

/// The last `capacity` failures, newest last. Entirely in memory: no state or log file is touched.
#[derive(Debug)]
pub struct FailureRing {
    state: Mutex<State>,
    /// Records not retained because the ring was disabled, full or contended.
    drops: AtomicU64,
}

impl Default for FailureRing {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl FailureRing {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.min(MAX_CAPACITY);
        Self {
            state: Mutex::new(State {
                capacity,
                records: VecDeque::with_capacity(capacity),
            }),
            drops: AtomicU64::new(0),
        }
    }

    /// Applies a config reload. Records beyond the new capacity are dropped, not moved to disk.
    pub fn set_capacity(&self, capacity: usize) {
        let capacity = capacity.min(MAX_CAPACITY);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let trimmed = state.records.len().saturating_sub(capacity);
        for _ in 0..trimmed {
            state.records.pop_front();
        }
        self.drops.fetch_add(trimmed as u64, Ordering::Relaxed);
        state.capacity = capacity;
    }

    /// Adds one record. Contention drops it: an observability buffer must never make a request
    /// wait. A capacity of zero disables the ring entirely.
    pub fn record(&self, record: FailureRecord) {
        let Ok(mut state) = self.state.try_lock() else {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if state.capacity == 0 {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if state.records.len() == state.capacity {
            state.records.pop_front();
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
        state.records.push_back(record);
    }

    /// Newest first, at most `limit`, optionally only failures for one key or stream.
    pub fn snapshot(
        &self,
        limit: usize,
        key: Option<&str>,
        stream: Option<&str>,
    ) -> Vec<FailureRecord> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .records
            .iter()
            .rev()
            .filter(|record| key.is_none_or(|key| record.key == key))
            .filter(|record| stream.is_none_or(|stream| record.stream == stream))
            .take(limit)
            .cloned()
            .collect()
    }

    /// How many records match a filter, before the response limit is applied.
    pub fn count_matching(&self, key: Option<&str>, stream: Option<&str>) -> usize {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .records
            .iter()
            .filter(|record| key.is_none_or(|key| record.key == key))
            .filter(|record| stream.is_none_or(|stream| record.stream == stream))
            .count()
    }

    /// How many records each failure class has in the ring; only observed classes.
    pub fn counts(&self) -> BTreeMap<String, u64> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = BTreeMap::new();
        for record in state
            .records
            .iter()
            .filter(|record| is_known_class(&record.class))
        {
            *out.entry(record.class.clone()).or_insert(0) += 1;
        }
        out
    }

    /// Host-owned Prometheus series, appended to `/v1/metrics` even without the metrics plugin.
    pub fn metrics(&self) -> String {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        format!(
            "# HELP reiki_failure_ring_entries Recent upstream failures kept in memory.\n\
                    # TYPE reiki_failure_ring_entries gauge\n\
                    reiki_failure_ring_entries {}\n\
                    # HELP reiki_failure_ring_dropped_total Failures not retained by the ring.\n\
                    # TYPE reiki_failure_ring_dropped_total counter\n\
                    reiki_failure_ring_dropped_total {}\n",
            state.records.len(),
            self.drops.load(Ordering::Relaxed)
        )
    }
}

/// Turns a `/v1/failures` answer into rows.
pub fn parse(body: &str) -> Result<Vec<FailureRecord>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("cannot read /v1/failures: {e}"))?;
    let rows = value
        .get("failures")
        .ok_or_else(|| "cannot read /v1/failures: missing failures".to_owned())?;
    serde_json::from_value(rows.clone()).map_err(|e| format!("cannot read /v1/failures: {e}"))
}

/// The JSON document `reiki failures --json` prints: the same envelope as the endpoint.
pub fn render_json(records: &[FailureRecord]) -> Result<String, String> {
    let body = serde_json::json!({
        "failures": records,
        "failures_total": records.len(),
        "failures_truncated": false,
    });
    serde_json::to_string(&body)
        .map(|text| format!("{text}\n"))
        .map_err(|e| format!("cannot write JSON: {e}"))
}

/// The table `reiki failures` prints, clipped to `width` characters per line.
pub fn render_table(records: &[FailureRecord], width: usize) -> String {
    if records.is_empty() {
        return "no failures\n".to_owned();
    }
    let headers = [
        "TIME", "STREAM", "KEY", "STATUS", "CLASS", "ACTION", "COOLED", "NOTE",
    ];
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
                record.action.label().to_owned(),
                match (record.cooled, record.cooldown_seconds) {
                    (true, Some(secs)) => format!("{secs}s"),
                    (true, None) => "yes".to_owned(),
                    (false, _) => "-".to_owned(),
                },
                record.note.clone().unwrap_or_default(),
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
    format_utc(secs, ' ', None, "")
}

fn write_time_at<S: Serializer>(secs: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&crate::reload::rfc3339_secs(*secs))
}

fn read_time_at<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    use time::format_description::well_known::Rfc3339;
    let text = String::deserialize(deserializer)?;
    let parsed = time::OffsetDateTime::parse(&text, &Rfc3339).map_err(serde::de::Error::custom)?;
    u64::try_from(parsed.unix_timestamp())
        .map_err(|_| serde::de::Error::custom("an instant before the Unix epoch"))
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
            action: FailureAction::Cooldown,
            retry_after: Some("120".to_owned()),
            cooled: true,
            cooldown_seconds: Some(10),
            note: Some(format!("failure {label}")),
        }
    }

    #[test]
    fn the_ring_keeps_the_newest_capacity_in_order() {
        let ring = FailureRing::new(8);
        for n in 0..13 {
            ring.record(failure(&n.to_string()));
        }
        let all = ring.snapshot(100, None, None);
        assert_eq!(all.len(), 8);
        assert_eq!(all.first().unwrap().key, "12");
        assert_eq!(all.last().unwrap().key, "5");
        assert_eq!(ring.drops.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn snapshot_is_newest_first_and_honours_limit_key_and_stream() {
        let ring = FailureRing::new(10);
        ring.record(failure("a"));
        let mut other_stream = failure("b");
        other_stream.stream = "t".to_owned();
        ring.record(other_stream);
        ring.record(failure("a"));
        let all = ring.snapshot(10, None, None);
        let keys: Vec<&str> = all.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["a", "b", "a"]);
        assert_eq!(ring.snapshot(1, None, None)[0].key, "a");
        let only = ring.snapshot(10, Some("a"), None);
        assert_eq!(only.len(), 2);
        assert!(only.iter().all(|r| r.key == "a"));
        let stream = ring.snapshot(10, None, Some("t"));
        assert_eq!(stream.len(), 1);
        assert_eq!(stream[0].key, "b");
        assert!(ring.snapshot(10, Some("a"), Some("t")).is_empty());
        assert!(ring.snapshot(10, Some("gone"), None).is_empty());
    }

    #[test]
    fn a_zero_capacity_ring_records_nothing_and_counts_drops() {
        let ring = FailureRing::new(0);
        ring.record(failure("a"));
        assert!(ring.snapshot(10, None, None).is_empty());
        assert!(ring.counts().is_empty());
        assert_eq!(ring.drops.load(Ordering::Relaxed), 1);
        ring.set_capacity(1);
        ring.record(failure("b"));
        assert_eq!(ring.snapshot(10, None, None).len(), 1);
    }

    #[test]
    fn shrinking_capacity_trims_the_oldest_records() {
        let ring = FailureRing::new(3);
        for n in 0..3 {
            ring.record(failure(&n.to_string()));
        }
        ring.set_capacity(1);
        let rows = ring.snapshot(10, None, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "2");
        assert_eq!(ring.drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn metrics_show_the_live_size_and_drops() {
        let ring = FailureRing::new(1);
        ring.record(failure("a"));
        ring.record(failure("b"));
        let text = ring.metrics();
        assert!(
            text.contains("# TYPE reiki_failure_ring_entries gauge"),
            "{text}"
        );
        assert!(text.contains("reiki_failure_ring_entries 1"), "{text}");
        assert!(
            text.contains("# TYPE reiki_failure_ring_dropped_total counter"),
            "{text}"
        );
        assert!(
            text.contains("reiki_failure_ring_dropped_total 1"),
            "{text}"
        );
    }

    #[test]
    fn actions_have_stable_wire_labels_and_default_to_pass_through() {
        assert_eq!(FailureAction::Cooldown.label(), "cooldown");
        assert_eq!(FailureAction::Rotate.label(), "rotate");
        assert_eq!(FailureAction::PassThrough.label(), "pass-through");
        let row = failure("a");
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"action\":\"cooldown\""), "{json}");
        let older = json.replace("\"action\":\"cooldown\",", "");
        let parsed: FailureRecord = serde_json::from_str(&older).unwrap();
        assert_eq!(parsed.action, FailureAction::PassThrough);
    }

    #[test]
    fn counts_name_only_the_classes_present() {
        let ring = FailureRing::new(10);
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
    fn counts_include_request_errors_and_ignore_unknown_labels() {
        let ring = FailureRing::new(10);
        let mut request = failure("a");
        request.class = "request_error".to_owned();
        ring.record(request);
        let mut unknown = failure("b");
        unknown.class = "invented".to_owned();
        ring.record(unknown);
        let counts = ring.counts();
        assert_eq!(counts.get("request_error"), Some(&1));
        assert!(!counts.contains_key("invented"));
        assert_eq!(counts.len(), 1);
    }

    #[test]
    fn the_table_has_the_columns_and_fits_the_width() {
        let text = render_table(&[failure("a")], 80);
        let header = text.lines().next().unwrap();
        for column in [
            "TIME", "STREAM", "KEY", "STATUS", "CLASS", "ACTION", "COOLED", "NOTE",
        ] {
            assert!(header.contains(column), "{header}");
        }
        assert!(text.contains("429"), "{text}");
        assert!(text.contains("cooldown"), "{text}");
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
