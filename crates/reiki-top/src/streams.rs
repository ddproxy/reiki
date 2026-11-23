//! Pure read-only view model for the Streams screen of `reiki top`.
//!
//! The admin listener supplies one snapshot: every configured stream (static and dynamic),
//! current usage against limits, token ids and expiries, and a bounded audit tail. This module
//! parses that answer and turns it into rows sorted by usage. It carries no token values, no
//! hashes and no clock: the caller supplies the current Unix time for countdowns.

use serde::Deserialize;

/// The admin listener's read-only stream answer.
///
/// Unknown fields are ignored on purpose: an older `reiki top` keeps working when the listener
/// adds fields, and a field named like a secret is simply dropped instead of being rendered.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamsAnswer {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub streams: Vec<StreamJson>,
    #[serde(default)]
    pub audit: Vec<AuditJson>,
}

/// One stream as the listener reports it. Only readable fields live here: a field that looks
/// like a credential is ignored by serde, before the view model can hold or render it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StreamJson {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default)]
    pub chain: Vec<String>,
    #[serde(default)]
    pub limits: LimitsJson,
    #[serde(default)]
    pub tokens: Vec<TokenJson>,
    #[serde(default)]
    pub expiry: Option<u64>,
    #[serde(default)]
    pub rpm: UsageJson,
    #[serde(default)]
    pub concurrency: UsageJson,
    #[serde(default)]
    pub last_used: Option<u64>,
    #[serde(default)]
    pub state: String,
}

/// Current and allowed usage of one resource.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct UsageJson {
    #[serde(default)]
    pub current: u64,
    #[serde(default)]
    pub limit: Option<u64>,
}

/// The configured ceilings of one stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LimitsJson {
    #[serde(default)]
    pub rpm: Option<u64>,
    #[serde(default)]
    pub tpm: Option<u64>,
    #[serde(default)]
    pub concurrent: Option<u64>,
    #[serde(default)]
    pub daily_requests: Option<u64>,
    #[serde(default)]
    pub daily_tokens: Option<u64>,
    #[serde(default)]
    pub monthly_requests: Option<u64>,
    #[serde(default)]
    pub monthly_tokens: Option<u64>,
}

impl LimitsJson {
    /// Fixed-order label/value pairs for the detail pane.
    pub fn lines(&self) -> Vec<(&'static str, String)> {
        [
            ("rpm", self.rpm),
            ("tpm", self.tpm),
            ("concurrent", self.concurrent),
            ("daily requests", self.daily_requests),
            ("daily tokens", self.daily_tokens),
            ("monthly requests", self.monthly_requests),
            ("monthly tokens", self.monthly_tokens),
        ]
        .into_iter()
        .map(|(label, value)| {
            (
                label,
                value.map_or_else(|| "-".to_owned(), |v| v.to_string()),
            )
        })
        .collect()
    }
}

/// One credential on a stream. The wire form has an id and an expiry, never a value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TokenJson {
    pub id: String,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub state: Option<String>,
}

impl TokenJson {
    /// The first eight characters of the id are enough to tell tokens apart in a narrow pane.
    pub fn short_id(&self) -> String {
        self.id.chars().take(8).collect()
    }

    /// Whether this token can still be used at `now`.
    pub fn active_at(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|expires| expires > now)
    }

    /// A fixed label for the detail pane; expiry wins over a stale server-side label.
    pub fn state_at(&self, now: u64) -> &'static str {
        if self.active_at(now) {
            "active"
        } else {
            "expired"
        }
    }
}

/// One management action. Deliberately only the fields an operator needs to read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AuditJson {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub stream: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub changes: Vec<ChangeJson>,
}

/// A field-level audit change. Both values were sanitized before they were written to the log.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChangeJson {
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

/// One stream as the screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub dynamic: bool,
    pub chain: Vec<String>,
    pub expiry: Option<u64>,
    pub rpm: UsageJson,
    pub concurrency: UsageJson,
    pub last_used: Option<u64>,
    pub state: String,
    pub limits: LimitsJson,
    pub tokens: Vec<TokenJson>,
}

impl Row {
    /// `dynamic` or `static`, for the table's type column.
    pub fn kind_label(&self) -> &'static str {
        if self.dynamic { "dynamic" } else { "static" }
    }

    /// The upstream chain, in the order requests try it.
    pub fn chain_label(&self) -> String {
        self.chain.join(" -> ")
    }

    /// Current and allowed request rate, such as `9/10` or `3/-`.
    pub fn rpm_label(&self) -> String {
        usage_label(&self.rpm)
    }

    /// Current and allowed concurrent requests.
    pub fn concurrency_label(&self) -> String {
        usage_label(&self.concurrency)
    }

    /// Time left before the soonest live token expires, or `-` when nothing expires.
    pub fn expiry_countdown(&self, now: u64) -> String {
        fmt_countdown(self.expiry, now)
    }

    /// How long ago the stream last admitted a request, or `-` when it never has.
    pub fn last_used_ago(&self, now: u64) -> String {
        fmt_ago(self.last_used, now)
    }
}

/// A compact `current/limit` pair; an absent limit is unlimited.
pub fn usage_label(usage: &UsageJson) -> String {
    match usage.limit {
        Some(limit) => format!("{}/{}", usage.current, limit),
        None => format!("{}/-", usage.current),
    }
}

/// A short countdown such as `4h02m`, `12m05s` or `45s`.
pub fn fmt_countdown(expires: Option<u64>, now: u64) -> String {
    match expires {
        None => "-".to_owned(),
        Some(expires) => fmt_parts(expires.saturating_sub(now)),
    }
}

/// A short age such as `4m`, or `-` for an unknown time.
pub fn fmt_ago(at: Option<u64>, now: u64) -> String {
    match at {
        Some(at) if at <= now => {
            let diff = now - at;
            if diff >= 3600 {
                format!("{}h", diff / 3600)
            } else if diff >= 60 {
                format!("{}m", diff / 60)
            } else {
                format!("{diff}s")
            }
        }
        Some(_) => "0s".to_owned(),
        None => "-".to_owned(),
    }
}

fn fmt_parts(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{secs:02}s")
    } else {
        format!("{secs}s")
    }
}

/// The whole screen's data, ready to render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamsView {
    /// False when `[stream_admin]` is not configured; dynamic data and audit are unavailable.
    pub enabled: bool,
    /// Rows in descending request-rate order, then concurrency, then name.
    pub rows: Vec<Row>,
    /// Recent audit records, newest first. Auditing a stream filters these by name.
    pub audit: Vec<AuditJson>,
}

impl StreamsView {
    /// Parses one admin answer. Malformed answers are an error, never a partial table.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let answer: StreamsAnswer = serde_json::from_str(text).map_err(|e| e.to_string())?;
        Ok(answer.into_view())
    }

    /// The selected row, or `None` when the list is empty or the index is stale.
    pub fn row(&self, index: Option<usize>) -> Option<&Row> {
        self.rows.get(index?)
    }

    /// Recent actions for one stream, newest first, in the order the listener sent them.
    pub fn audit_for<'a>(&'a self, stream: &str) -> impl Iterator<Item = &'a AuditJson> {
        self.audit.iter().filter(move |a| a.stream == stream)
    }
}

impl StreamsAnswer {
    /// Sorts by usage and drops everything the screen must never display.
    pub fn into_view(self) -> StreamsView {
        let mut rows: Vec<Row> = self
            .streams
            .into_iter()
            .map(|stream| {
                let tokens = stream.tokens;
                let expiry = stream
                    .expiry
                    .or_else(|| tokens.iter().filter_map(|token| token.expires_at).min());
                let state = if stream.state.is_empty() {
                    if stream.dynamic && tokens.is_empty() {
                        "revoked".to_owned()
                    } else {
                        "active".to_owned()
                    }
                } else {
                    stream.state
                };
                Row {
                    name: stream.name,
                    dynamic: stream.dynamic,
                    chain: stream.chain,
                    expiry,
                    rpm: stream.rpm,
                    concurrency: stream.concurrency,
                    last_used: stream.last_used,
                    state,
                    limits: stream.limits,
                    tokens,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.rpm
                .current
                .cmp(&a.rpm.current)
                .then_with(|| b.concurrency.current.cmp(&a.concurrency.current))
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut audit = self.audit;
        audit.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| b.time.cmp(&a.time)));
        StreamsView {
            enabled: self.enabled,
            rows,
            audit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built at runtime: no credential-shaped literal in the source.
    fn secret() -> String {
        ["rsk", "_", &"f".repeat(64)].concat()
    }

    fn fixture(secret: &str) -> String {
        format!(
            r#"{{
              "enabled": true,
              "streams": [
                {{
                  "name": "quiet", "dynamic": false,
                  "chain": ["zen", "openrouter"],
                  "limits": {{"rpm": 30, "tpm": null, "concurrent": 2,
                    "daily_requests": null, "daily_tokens": null,
                    "monthly_requests": null, "monthly_tokens": null}},
                  "tokens": [], "expiry": null,
                  "rpm": {{"current": 1, "limit": 30}},
                  "concurrency": {{"current": 0, "limit": 2}},
                  "last_used": 1000, "state": "active"
                }},
                {{
                  "name": "busy", "dynamic": true,
                  "chain": ["zen"], "limits": {{"rpm": 10}},
                  "tokens": [
                    {{"id": "abcdef1234567890", "expires_at": 1900, "state": "active"}},
                    {{"id": "9999888877776666", "expires_at": 1200}}
                  ],
                  "expiry": 1200,
                  "rpm": {{"current": 9, "limit": 10}},
                  "concurrency": {{"current": 2, "limit": 2}},
                  "last_used": 1150, "state": "active",
                  "hash": "not shown", "token": "{secret}"
                }}
              ],
              "audit": [
                {{"seq": 2, "time": "2026-11-23T00:01:00Z", "action": "token.add",
                  "stream": "busy", "outcome": "success", "reason": "rotation",
                  "changes": [{{"field": "token.id", "from": "-", "to": "abcdef12"}}]}},
                {{"seq": 1, "time": "2026-11-23T00:00:00Z", "action": "create",
                  "stream": "busy", "outcome": "success", "reason": "new tenant",
                  "changes": []}},
                {{"seq": 3, "time": "2026-11-23T00:02:00Z", "action": "create",
                  "stream": "quiet", "outcome": "success", "reason": "earlier config",
                  "changes": []}}
              ]
            }}"#
        )
    }

    #[test]
    fn fixture_json_builds_rows_sorted_by_usage() {
        let view = StreamsView::from_json(&fixture(&secret())).expect("fixture parses");
        assert!(view.enabled);
        assert_eq!(
            view.rows
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["busy", "quiet"]
        );
        let busy = &view.rows[0];
        assert!(busy.dynamic);
        assert_eq!(busy.chain, ["zen"]);
        assert_eq!((busy.rpm.current, busy.rpm.limit), (9, Some(10)));
        assert_eq!(
            (busy.concurrency.current, busy.concurrency.limit),
            (2, Some(2))
        );
        assert_eq!(busy.expiry, Some(1200));
        assert_eq!(busy.limits.rpm, Some(10));
        assert_eq!(busy.tokens.len(), 2);
        assert_eq!(busy.kind_label(), "dynamic");
        assert_eq!(busy.chain_label(), "zen");
        assert_eq!(busy.rpm_label(), "9/10");
        assert_eq!(busy.concurrency_label(), "2/2");
        assert_eq!(view.rows[1].rpm_label(), "1/30");
        assert_eq!(busy.limits.lines()[1], ("tpm", "-".to_owned()));
    }

    #[test]
    fn audit_is_newest_first_and_filtered_by_stream() {
        let view = StreamsView::from_json(&fixture(&secret())).expect("fixture parses");
        let busy: Vec<u64> = view.audit_for("busy").map(|a| a.seq).collect();
        assert_eq!(busy, [2, 1]);
        assert_eq!(
            view.audit_for("quiet").map(|a| a.seq).collect::<Vec<_>>(),
            [3]
        );
    }

    #[test]
    fn token_values_hashes_and_unknown_fields_are_never_kept() {
        let secret = secret();
        let view = StreamsView::from_json(&fixture(&secret)).expect("fixture parses");
        let rendered = format!("{view:?}");
        assert!(
            !rendered.contains(&secret),
            "no token value in the view model"
        );
        assert!(!rendered.contains("not shown"), "no hash in the view model");
        let token = &view.rows[0].tokens[0];
        assert_eq!(token.short_id(), "abcdef12");
        assert_eq!(token.state_at(1900), "expired");
        assert_eq!(token.state_at(1000), "active");
    }

    #[test]
    fn countdowns_and_missing_times_are_safe() {
        let view = StreamsView::from_json(&fixture(&secret())).expect("fixture parses");
        let busy = &view.rows[0];
        assert_eq!(busy.expiry_countdown(1150), "50s");
        assert_eq!(busy.last_used_ago(1155), "5s");
        assert_eq!(view.rows[1].expiry_countdown(1150), "-");
        assert_eq!(fmt_countdown(None, 0), "-");
        assert_eq!(fmt_ago(None, 10), "-");
    }

    #[test]
    fn disabled_answer_keeps_static_rows_and_drops_dynamic_fields() {
        let view = StreamsView::from_json(
            r#"{"enabled":false,"streams":[{"name":"local","dynamic":false,
                 "chain":["zen"],"limits":{},"tokens":[],"rpm":{"current":0},
                 "concurrency":{"current":0},"last_used":null,"state":"active",
                 "token":"value-that-must-not-appear"}]}"#,
        )
        .expect("disabled answer parses");
        assert!(!view.enabled);
        assert_eq!(view.rows.len(), 1);
        assert!(view.audit.is_empty());
        assert!(!format!("{view:?}").contains("value-that-must-not-appear"));
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        for text in ["", "{", "not json", r#"{"streams":"many"}"#] {
            assert!(StreamsView::from_json(text).is_err(), "{text}");
        }
    }
}
