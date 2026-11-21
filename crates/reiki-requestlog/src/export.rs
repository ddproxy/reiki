//! Summary-only export to a rotating file or a syslog collector.
//!
//! The export is independent of the main request log. It carries no bodies, header
//! values, query strings or credential values: only the field names listed in
//! [`render`]. A bounded queue keeps a full collector from delaying a request; a
//! record that cannot be queued or sent is counted in
//! `reiki_requestlog_export_dropped_total`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use http::HeaderMap;
use serde_json::{json, Value};

use crate::config::ExportConfig;
use crate::redact;
use crate::sink::{FileSink, Notice};
use crate::syslog::{Meta, Syslog, SEVERITY_INFO, SEVERITY_WARNING};
use crate::timefmt::rfc3339_millis;

/// Longest scalar field in an export record, in bytes.
const MAX_FIELD_BYTES: usize = 256;
/// Longest redacted route, in bytes, so a UDP datagram stays well bounded.
const MAX_ROUTE_BYTES: usize = 512;
/// Most response body bytes inspected for token usage.
const MAX_JSON_BUFFER: usize = 64 * 1024;
/// Longest event-stream line inspected for token usage.
const MAX_SSE_LINE: usize = 16 * 1024;
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

/// The counter is still published when export is off, so scrapers see a stable series.
pub(crate) fn empty_metrics() -> String {
    metrics_with(0)
}

fn metrics_with(count: u64) -> String {
    format!(
        "# HELP reiki_requestlog_export_dropped_total Summary export records dropped.\n\
         # TYPE reiki_requestlog_export_dropped_total counter\n\
         reiki_requestlog_export_dropped_total {count}\n"
    )
}

/// The stable field names of one summary. Values are already safe to write: no
/// body, header value, query string or credential value is carried here.
pub(crate) struct Summary {
    pub(crate) at: SystemTime,
    pub(crate) id: String,
    pub(crate) method: String,
    pub(crate) route: String,
    pub(crate) upstream: String,
    pub(crate) status: Option<u16>,
    pub(crate) duration_ms: u64,
    pub(crate) head_ms: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) key: Option<String>,
    pub(crate) failure_class: Option<&'static str>,
}

pub(crate) enum Item {
    Summary(Summary),
    Shutdown,
}

enum Sink {
    File(FileSink),
    Syslog(Syslog),
}

/// The writer thread and the counters behind the export.
pub(crate) struct Export {
    pub(crate) tx: SyncSender<Item>,
    pub(crate) dropped: Arc<AtomicU64>,
    pub(crate) warned: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

impl Export {
    pub(crate) fn new(config: &ExportConfig) -> Result<Self, String> {
        let sink = match config.format {
            crate::config::ExportFormat::Jsonl => {
                let path = config.file.as_ref().ok_or_else(|| {
                    "requestlog: export needs `file` for the jsonl format".to_owned()
                })?;
                Sink::File(
                    FileSink::open(path, config.max_bytes, config.max_files).map_err(|e| {
                        format!(
                            "requestlog: cannot open export file {}: {e}",
                            path.display()
                        )
                    })?,
                )
            }
            crate::config::ExportFormat::Syslog => {
                let target = config.syslog.as_ref().ok_or_else(|| {
                    "requestlog: export needs `syslog` for the syslog format".to_owned()
                })?;
                #[cfg(not(unix))]
                if matches!(target, crate::SyslogTarget::Unix(_)) {
                    return Err(
                        "requestlog: a unix export syslog target needs a Unix platform".to_owned(),
                    );
                }
                Sink::Syslog(Syslog::new(
                    target.clone(),
                    config.syslog_facility,
                    config.syslog_app_name.clone(),
                ))
            }
        };
        let (tx, rx) = sync_channel(config.queue);
        let dropped = Arc::new(AtomicU64::new(0));
        let warned = Arc::new(AtomicBool::new(false));
        let writer_dropped = Arc::clone(&dropped);
        let writer_warned = Arc::clone(&warned);
        let writer = thread::Builder::new()
            .name("requestlog-export".to_owned())
            .spawn(move || write_loop(sink, rx, writer_dropped, writer_warned))
            .map_err(|e| format!("requestlog: cannot start the export writer: {e}"))?;
        Ok(Self {
            tx,
            dropped,
            warned,
            writer: Some(writer),
        })
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub(crate) fn metrics(&self) -> String {
        metrics_with(self.dropped())
    }

    /// Sends the shutdown item and waits briefly for the writer to drain.
    pub(crate) fn flush(&self) {
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        let mut item = Some(Item::Shutdown);
        while let Some(pending) = item.take() {
            match self.tx.try_send(pending) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                    item = Some(back);
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
        if let Some(writer) = &self.writer {
            while !writer.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            let _ = self.tx.send(Item::Shutdown);
            let _ = writer.join();
        }
    }
}

/// Queues one summary without ever waiting for the writer.
pub(crate) fn send(
    tx: &SyncSender<Item>,
    dropped: &AtomicU64,
    warned: &AtomicBool,
    summary: Summary,
) {
    match tx.try_send(Item::Summary(summary)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => drop_record(dropped, warned, "the export queue is full"),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn drop_record(dropped: &AtomicU64, warned: &AtomicBool, reason: &'static str) {
    dropped.fetch_add(1, Ordering::Relaxed);
    if !warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(%reason, "requestlog: dropping a summary export record");
    }
}

fn write_loop(
    mut sink: Sink,
    rx: std::sync::mpsc::Receiver<Item>,
    dropped: Arc<AtomicU64>,
    warned: Arc<AtomicBool>,
) {
    for item in rx {
        let summary = match item {
            Item::Summary(summary) => summary,
            Item::Shutdown => break,
        };
        let line = render(&summary);
        match &mut sink {
            Sink::File(file) => {
                if let Err(e) = file.write_line(&line) {
                    tracing::warn!(error = %e, "requestlog: cannot write an export record");
                }
                match file.take_notice() {
                    Some(Notice::RotateFailed(e)) => {
                        tracing::warn!(error = %e, "requestlog: export rotation failed");
                    }
                    Some(Notice::RotateRecovered) => {
                        tracing::info!("requestlog: export rotation recovered");
                    }
                    None => {}
                }
            }
            Sink::Syslog(syslog) => {
                let failed =
                    summary.status.is_some_and(|s| s >= 500) || summary.failure_class.is_some();
                let meta = Meta {
                    msgid: "requestlog",
                    severity: if failed {
                        SEVERITY_WARNING
                    } else {
                        SEVERITY_INFO
                    },
                    at: summary.at,
                };
                if !syslog.send(&meta, &line) {
                    drop_record(
                        &dropped,
                        &warned,
                        "the syslog collector refused a summary record",
                    );
                }
            }
        }
    }
}

/// Renders the one stable export shape, shared by JSONL and syslog delivery.
pub(crate) fn render(summary: &Summary) -> String {
    json!({
        "v": 1,
        "event": "request_summary",
        "ts": rfc3339_millis(summary.at),
        "id": field(&summary.id),
        "method": field(&summary.method),
        "route": field(&summary.route),
        "upstream": field(&summary.upstream),
        "status": summary.status,
        "duration_ms": summary.duration_ms,
        "head_ms": summary.head_ms,
        "input_tokens": summary.input_tokens,
        "output_tokens": summary.output_tokens,
        "key": summary.key.as_deref().map(field),
        "failure_class": summary.failure_class,
    })
    .to_string()
}

/// Keeps a field bounded and free of characters that could forge another syslog line.
fn field(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if out.len() + c.len_utf8() > MAX_FIELD_BYTES {
            break;
        }
        out.push(if c.is_control() { '?' } else { c });
    }
    out
}

/// Removes the query and fragment defensively, then masks credential-shaped segments.
/// Reiki's `RequestCtx::path` already has no query string; this guards other callers.
pub(crate) fn route(path: &str) -> String {
    let bare = path.split(['?', '#']).next().unwrap_or(path);
    let redacted = redact::path(bare);
    let mut out = String::new();
    for c in redacted.chars() {
        if out.len() + c.len_utf8() > MAX_ROUTE_BYTES {
            break;
        }
        out.push(if c.is_control() { '?' } else { c });
    }
    out
}

/// True for `text/event-stream`, ignoring case and parameters.
pub(crate) fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        })
}

/// Stable label for the failure that ended a request, or `None` for a success.
pub(crate) fn failure_class(status: Option<u16>, attempts: &[Option<u16>]) -> Option<&'static str> {
    if status.is_some_and(|code| (200..400).contains(&code)) {
        return None;
    }
    let classify = |code: u16| match code {
        429 => Some("rate_limited"),
        402 => Some("quota_exhausted"),
        401 | 403 => Some("unauthorized"),
        500..=599 => Some("transient"),
        400..=499 => Some("request_error"),
        _ => None,
    };
    for attempt in attempts {
        match attempt {
            Some(code) => {
                if let Some(label) = classify(*code) {
                    return Some(label);
                }
            }
            None => return Some("transport_error"),
        }
    }
    match status {
        Some(code) => classify(code).or(Some("request_error")),
        None => Some("transport_error"),
    }
}

/// Bounded token counter. Only the numbers are kept; response text never leaves here.
#[derive(Debug, Default)]
pub(crate) struct Usage {
    is_sse: bool,
    buffer: Vec<u8>,
    discarding: bool,
    exceeded: bool,
    input: Option<u64>,
    output: Option<u64>,
    done: bool,
}

impl Usage {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_sse(&mut self, is_sse: bool) {
        self.is_sse = is_sse;
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        if self.is_sse {
            self.feed_sse(chunk);
        } else if !self.exceeded {
            if self.buffer.len() + chunk.len() > MAX_JSON_BUFFER {
                self.exceeded = true;
                self.buffer.clear();
            } else {
                self.buffer.extend_from_slice(chunk);
            }
        }
    }

    fn feed_sse(&mut self, chunk: &[u8]) {
        for part in chunk.split_inclusive(|b| *b == b'\n') {
            let complete = part.ends_with(b"\n");
            if !self.discarding {
                if self.buffer.len() + part.len() > MAX_SSE_LINE + usize::from(complete) {
                    self.buffer.clear();
                    self.discarding = true;
                } else {
                    self.buffer.extend_from_slice(part);
                }
            }
            if complete {
                if !self.discarding {
                    let line = std::mem::take(&mut self.buffer);
                    self.read_event_line(&line);
                }
                self.buffer.clear();
                self.discarding = false;
            }
        }
    }

    fn read_event_line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let Some(data) = text.trim().strip_prefix("data:") else {
            return;
        };
        if !data.contains("\"usage\"") {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
            self.take(usage_in(&value));
        }
    }

    fn take(&mut self, (input, output): (Option<u64>, Option<u64>)) {
        self.input = input.or(self.input);
        self.output = output.or(self.output);
    }

    /// Ends the count once, even when the state is dropped after a final chunk.
    pub(crate) fn finish(&mut self) -> (u64, u64) {
        if std::mem::replace(&mut self.done, true) {
            return (self.input.unwrap_or(0), self.output.unwrap_or(0));
        }
        if self.is_sse {
            let rest = std::mem::take(&mut self.buffer);
            if !self.discarding {
                self.read_event_line(&rest);
            }
        } else if !self.exceeded {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.buffer) {
                self.take(usage_in(&value));
            }
        }
        (self.input.unwrap_or(0), self.output.unwrap_or(0))
    }
}

/// OpenAI (`prompt_tokens`) and Anthropic (`input_tokens`) naming, with Anthropic's
/// `message_start` usage nested under `message`.
fn usage_in(value: &Value) -> (Option<u64>, Option<u64>) {
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"));
    let Some(usage) = usage else {
        return (None, None);
    };
    let count = |names: [&str; 2]| names.iter().find_map(|n| usage.get(*n)?.as_u64());
    (
        count(["prompt_tokens", "input_tokens"]),
        count(["completion_tokens", "output_tokens"]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reiki-export-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn summary(tag: &str) -> Summary {
        Summary {
            at: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            id: "abc123".to_owned(),
            method: "POST".to_owned(),
            route: format!("/v1/chat/{tag}"),
            upstream: "zen".to_owned(),
            status: Some(200),
            duration_ms: 42,
            head_ms: 12,
            input_tokens: 7,
            output_tokens: 9,
            key: Some("zen-primary".to_owned()),
            failure_class: None,
        }
    }

    fn config(text: &str) -> ExportConfig {
        Config::parse(&text.parse().unwrap())
            .unwrap()
            .export
            .unwrap()
    }

    #[test]
    fn render_uses_only_the_documented_field_names() {
        let allowed = [
            "v",
            "event",
            "ts",
            "id",
            "method",
            "route",
            "upstream",
            "status",
            "duration_ms",
            "head_ms",
            "input_tokens",
            "output_tokens",
            "key",
            "failure_class",
        ];
        let mut s = summary("mode");
        s.route = "/v1/chat".to_owned();
        let line = render(&s);
        let value: Value = serde_json::from_str(&line).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), allowed.len());
        assert!(keys.iter().all(|key| allowed.contains(key)), "{keys:?}");
        assert_eq!(value["event"], "request_summary");
        assert_eq!(value["failure_class"], Value::Null);
        assert!(!line.contains("body"), "{line}");
        assert!(!line.contains("header"), "{line}");
        assert!(!line.contains("query"), "{line}");
    }

    #[test]
    fn route_drops_query_and_fragment_and_masks_secrets() {
        assert_eq!(route("/v1/models?key=sk-secret&x=1"), "/v1/models");
        assert_eq!(route("/v1/models#fragment"), "/v1/models");
        assert!(route("/v1/users/sk-0123456789abcdef0123456789").contains("[REDACTED]"));
        let s = summary("x");
        let line = render(&Summary {
            route: route("/v1/chat?token=sk-0123456789abcdef0123456789"),
            ..s
        });
        assert!(!line.contains("sk-"), "{line}");
    }

    #[test]
    fn failure_classes_are_stable_and_success_has_none() {
        assert_eq!(failure_class(Some(200), &[Some(500)]), None);
        assert_eq!(failure_class(Some(429), &[]), Some("rate_limited"));
        assert_eq!(failure_class(Some(402), &[]), Some("quota_exhausted"));
        assert_eq!(failure_class(Some(401), &[]), Some("unauthorized"));
        assert_eq!(failure_class(Some(503), &[Some(429)]), Some("rate_limited"));
        assert_eq!(failure_class(Some(500), &[]), Some("transient"));
        assert_eq!(failure_class(Some(404), &[]), Some("request_error"));
        assert_eq!(failure_class(None, &[None]), Some("transport_error"));
        assert_eq!(failure_class(None, &[]), Some("transport_error"));
    }

    #[test]
    fn a_full_export_queue_counts_a_drop_without_blocking() {
        let (tx, rx) = sync_channel::<Item>(1);
        tx.send(Item::Shutdown).unwrap();
        let dropped = AtomicU64::new(0);
        let warned = AtomicBool::new(false);
        let started = Instant::now();
        for n in 0..100 {
            send(&tx, &dropped, &warned, summary(&n.to_string()));
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(dropped.load(Ordering::Relaxed), 100);
        assert!(warned.load(Ordering::Relaxed));
        drop(rx);
    }

    #[test]
    fn file_export_rotates_and_caps_the_number_of_files() {
        let dir = dir("rotate");
        let path = dir.join("export.jsonl");
        let config = config(&format!(
            "export = {{ format = \"jsonl\", file = \"{}\", max_bytes = 200, max_files = 2 }}",
            path.display()
        ));
        let export = Export::new(&config).unwrap();
        for n in 0..7 {
            send(
                &export.tx,
                &export.dropped,
                &export.warned,
                summary(&n.to_string()),
            );
        }
        export.flush();
        drop(export);
        assert!(path.exists());
        assert!(path.with_extension("jsonl.1").exists());
        assert!(path.with_extension("jsonl.2").exists());
        assert!(!path.with_extension("jsonl.3").exists());
        let all: String = [
            path.clone(),
            path.with_extension("jsonl.1"),
            path.with_extension("jsonl.2"),
        ]
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
        // The cap deletes the oldest file, so only the newest records survive.
        let kept = all.lines().count();
        assert!((1..=7).contains(&kept), "{kept} lines kept");
        assert!(all
            .lines()
            .all(|line| serde_json::from_str::<Value>(line).is_ok()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn udp_export_sends_one_rfc5424_summary_per_record() {
        let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let config = config(&format!(
            "export = {{ format = \"syslog\", syslog = \"udp://{}\", syslog_facility = \"local0\", syslog_app_name = \"reiki-export\" }}",
            peer.local_addr().unwrap()
        ));
        let export = Export::new(&config).unwrap();
        send(&export.tx, &export.dropped, &export.warned, summary("udp"));
        export.flush();
        drop(export);

        let mut buf = [0u8; 8 * 1024];
        let (n, _) = peer.recv_from(&mut buf).unwrap();
        let message = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(message.starts_with("<134>1 "), "{message}");
        assert!(
            message.contains(" reiki-export ") && message.contains(" requestlog - "),
            "{message}"
        );
        let body = message.rsplit_once(" - ").unwrap().1;
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["event"], "request_summary");
        assert_eq!(value["id"], "abc123");
    }

    #[test]
    fn token_usage_is_parsed_from_json_and_event_streams() {
        let mut json = Usage::new();
        json.feed(br#"{"usage":{"prompt_tokens":10,"completion_tokens":20}}"#);
        assert_eq!(json.finish(), (10, 20));
        assert_eq!(json.finish(), (10, 20), "finishing twice changes nothing");

        let mut sse = Usage::new();
        sse.set_sse(true);
        sse.feed(b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}\n\n");
        assert_eq!(sse.finish(), (3, 4));

        let mut split = Usage::new();
        split.set_sse(true);
        for part in [
            "data: {\"usage\":{\"prompt_",
            "tokens\":7,\"completion_tokens\":8}}\n\n",
        ] {
            split.feed(part.as_bytes());
        }
        assert_eq!(split.finish(), (7, 8));
    }

    #[test]
    fn metrics_names_the_drop_counter() {
        let dir = dir("metrics");
        let config = config(&format!(
            "export = {{ format = \"jsonl\", file = \"{}\" }}",
            dir.join("export.jsonl").display()
        ));
        let export = Export::new(&config).unwrap();
        let text = export.metrics();
        assert!(
            text.contains("# TYPE reiki_requestlog_export_dropped_total counter"),
            "{text}"
        );
        assert!(
            text.contains("reiki_requestlog_export_dropped_total 0\n"),
            "{text}"
        );
        export.flush();
        drop(export);
        let _ = std::fs::remove_dir_all(dir);
    }
}
