use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::config::{BodyMode, Config};
use crate::redact;
use crate::sink::FileSink;
#[cfg(unix)]
use crate::sink::SocketHub;
use crate::timefmt::rfc3339_millis;

const MAX_HEADER_VALUE: usize = 256;

pub enum Event {
    Request(RequestEvent),
    Response(ResponseEvent),
    Shutdown,
}

pub struct RequestEvent {
    pub id: String,
    pub at: SystemTime,
    pub stream: String,
    pub upstream: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body_bytes: usize,
    pub body: Option<Bytes>,
}

pub struct ResponseEvent {
    pub id: String,
    pub at: SystemTime,
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub bytes: u64,
}

/// Sinks owned by the writer thread.
pub struct Sinks {
    pub file: Option<FileSink>,
    #[cfg(unix)]
    pub socket: Option<SocketHub>,
}

pub fn spawn(
    config: Arc<Config>,
    mut sinks: Sinks,
    rx: Receiver<Event>,
    dropped: Arc<AtomicU64>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("requestlog-writer".to_owned())
        .spawn(move || {
            let mut file_failing = false;
            for event in rx {
                if matches!(event, Event::Shutdown) {
                    break;
                }
                let line = render(&config, event);
                emit(&mut sinks, &line, &mut file_failing);
                let lost = dropped.swap(0, Ordering::Relaxed);
                if lost > 0 {
                    let note = json!({
                        "v": 1,
                        "event": "dropped",
                        "ts": rfc3339_millis(SystemTime::now()),
                        "count": lost,
                    })
                    .to_string();
                    emit(&mut sinks, &note, &mut file_failing);
                }
            }
        })
}

fn emit(sinks: &mut Sinks, line: &str, file_failing: &mut bool) {
    if let Some(file) = sinks.file.as_mut() {
        match file.write_line(line) {
            Ok(()) if *file_failing => {
                *file_failing = false;
                tracing::info!("requestlog: file sink recovered");
            }
            Ok(()) => {}
            Err(e) if !*file_failing => {
                *file_failing = true;
                tracing::warn!(error = %e, "requestlog: cannot write the log file");
            }
            Err(_) => {}
        }
    }
    #[cfg(unix)]
    if let Some(socket) = sinks.socket.as_ref() {
        socket.broadcast(&Arc::from(line));
    }
}

pub fn render(config: &Config, event: Event) -> String {
    match event {
        Event::Request(request) => render_request(config, request),
        Event::Shutdown => String::new(),
        Event::Response(response) => json!({
            "v": 1,
            "event": "response",
            "id": response.id,
            "ts": rfc3339_millis(response.at),
            "status": response.status,
            "duration_ms": response.duration_ms,
            "bytes": response.bytes,
        })
        .to_string(),
    }
}

fn render_request(config: &Config, request: RequestEvent) -> String {
    let mut record = Map::new();
    record.insert("v".into(), json!(1));
    record.insert("event".into(), json!("request"));
    record.insert("id".into(), json!(request.id));
    record.insert("ts".into(), json!(rfc3339_millis(request.at)));
    record.insert("stream".into(), json!(request.stream));
    record.insert("upstream".into(), json!(request.upstream));
    record.insert("method".into(), json!(request.method));

    if !request.headers.is_empty() {
        let headers: Map<String, Value> = request
            .headers
            .iter()
            .map(|(name, value)| {
                let value = truncate(value, MAX_HEADER_VALUE);
                let value = if config.redact {
                    redact::secrets(&value).into_owned()
                } else {
                    value
                };
                (name.clone(), Value::String(value))
            })
            .collect();
        record.insert("headers".into(), Value::Object(headers));
    }

    record.insert("body_bytes".into(), json!(request.body_bytes));
    if let Some(body) = request.body.filter(|b| !b.is_empty()) {
        match config.body {
            BodyMode::None => {}
            BodyMode::Summary => {
                if let Some(summary) = summarize(&body) {
                    record.insert("body_summary".into(), summary);
                }
            }
            BodyMode::Full => {
                let (value, truncated) = full_body(config, &body);
                record.insert("body".into(), value);
                if truncated {
                    record.insert("body_truncated".into(), json!(true));
                }
            }
        }
    }
    Value::Object(record).to_string()
}

fn full_body(config: &Config, body: &[u8]) -> (Value, bool) {
    let masked = |text: &str| {
        if config.redact {
            redact::secrets(text).into_owned()
        } else {
            text.to_owned()
        }
    };
    match serde_json::from_slice::<Value>(body) {
        Ok(mut value) => {
            if config.redact {
                redact::json(&mut value);
            }
            let text = value.to_string();
            if text.len() <= config.max_body_bytes {
                (value, false)
            } else {
                (Value::String(truncate(&text, config.max_body_bytes)), true)
            }
        }
        Err(_) => {
            let text = masked(&String::from_utf8_lossy(body));
            let truncated = text.len() > config.max_body_bytes;
            (
                Value::String(truncate(&text, config.max_body_bytes)),
                truncated,
            )
        }
    }
}

fn summarize(body: &[u8]) -> Option<Value> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    let mut summary = Map::new();
    for key in ["model", "stream", "max_tokens", "temperature"] {
        if let Some(found) = object.get(key) {
            if found.is_string() || found.is_number() || found.is_boolean() {
                summary.insert(key.to_owned(), found.clone());
            }
        }
    }
    for (key, name) in [
        ("messages", "messages"),
        ("tools", "tools"),
        ("input", "input_items"),
    ] {
        if let Some(Value::Array(items)) = object.get(key) {
            summary.insert(name.to_owned(), json!(items.len()));
        }
    }
    Some(Value::Object(summary))
}

/// Cuts at a character boundary at or below `max` bytes.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn config(extra: &str) -> Config {
        let text = format!("file = \"/x\"\n{extra}");
        Config::parse(&text.parse().unwrap()).unwrap()
    }

    fn request(body: &str) -> RequestEvent {
        RequestEvent {
            id: "abc".to_owned(),
            at: UNIX_EPOCH + Duration::from_secs(951_782_400),
            stream: "zen".to_owned(),
            upstream: "zen".to_owned(),
            method: "POST".to_owned(),
            headers: vec![("user-agent".to_owned(), "opencode/1.18.35".to_owned())],
            body_bytes: body.len(),
            body: Some(Bytes::from(body.to_owned())),
        }
    }

    fn parsed(config: &Config, event: RequestEvent) -> Value {
        serde_json::from_str(&render(config, Event::Request(event))).unwrap()
    }

    #[test]
    fn a_full_json_body_is_embedded_as_json_with_secrets_masked() {
        let body =
            r#"{"model":"big-pickle","messages":[{"role":"user","content":"key TOKEN ok"}]}"#
                .replace("TOKEN", &["sk-", "0123456789abcdef0123456789"].concat());
        let record = parsed(&config(""), request(&body));
        assert_eq!(record["event"], "request");
        assert_eq!(record["ts"], "2000-02-29T00:00:00.000Z");
        assert_eq!(record["headers"]["user-agent"], "opencode/1.18.35");
        assert_eq!(record["body"]["model"], "big-pickle");
        assert_eq!(
            record["body"]["messages"][0]["content"],
            "key [REDACTED] ok"
        );
        assert!(record.get("body_truncated").is_none());
        assert_eq!(record["body_bytes"], body.len());
    }

    #[test]
    fn redaction_can_be_turned_off() {
        let token = ["sk-", "0123456789abcdef0123456789"].concat();
        let body = format!(r#"{{"content":"{token}"}}"#);
        let record = parsed(&config("redact = false"), request(&body));
        assert_eq!(record["body"]["content"], token);
    }

    #[test]
    fn an_oversized_body_is_cut_on_a_character_boundary_and_flagged() {
        let body = format!(r#"{{"content":"{}"}}"#, "é".repeat(100));
        let record = parsed(&config("max_body_bytes = 31"), request(&body));
        let text = record["body"].as_str().unwrap();
        assert!(
            text.len() <= 31 && text.starts_with(r#"{"content":"é"#),
            "{text}"
        );
        assert_eq!(record["body_truncated"], true);
    }

    #[test]
    fn a_body_that_is_not_json_is_logged_as_masked_text() {
        let record = parsed(
            &config(""),
            request("plain Bearer abcdef0123456789abcdef text"),
        );
        assert_eq!(record["body"], "plain Bearer [REDACTED] text");
    }

    #[test]
    fn summary_mode_keeps_counts_and_no_prompt_text() {
        let body = r#"{"model":"m","stream":true,"messages":[{"content":"secret prompt"},{}],"tools":[1,2,3]}"#;
        let record = parsed(&config("body = \"summary\""), request(body));
        assert_eq!(record["body_summary"]["messages"], 2);
        assert_eq!(record["body_summary"]["tools"], 3);
        assert_eq!(record["body_summary"]["model"], "m");
        assert!(!record.to_string().contains("secret prompt"));
        assert!(record.get("body").is_none());
    }

    #[test]
    fn none_mode_omits_the_body_but_keeps_its_size() {
        let record = parsed(&config("body = \"none\""), request(r#"{"a":1}"#));
        assert!(record.get("body").is_none() && record.get("body_summary").is_none());
        assert_eq!(record["body_bytes"], 7);
    }

    #[test]
    fn header_values_are_masked_and_capped() {
        let mut event = request("{}");
        event.headers = vec![
            (
                "x-trace".to_owned(),
                "Bearer abcdef0123456789abcdef".to_owned(),
            ),
            ("x-long".to_owned(), "a".repeat(1000)),
        ];
        let record = parsed(&config(""), event);
        assert_eq!(record["headers"]["x-trace"], "Bearer [REDACTED]");
        assert_eq!(record["headers"]["x-long"].as_str().unwrap().len(), 256);
    }

    #[test]
    fn a_dropped_count_is_written_after_the_next_record() {
        let dir =
            std::env::temp_dir().join(format!("reiki-requestlog-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("req.jsonl");
        let config = Arc::new(config("queue = 16"));
        let sinks = Sinks {
            file: Some(FileSink::open(&path, 0, 0).unwrap()),
            #[cfg(unix)]
            socket: None,
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let dropped = Arc::new(AtomicU64::new(3));
        let handle = spawn(config, sinks, rx, Arc::clone(&dropped)).unwrap();
        tx.send(Event::Request(request("{}"))).unwrap();
        tx.send(Event::Shutdown).unwrap();
        handle.join().unwrap();

        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "request");
        assert_eq!(lines[1]["event"], "dropped");
        assert_eq!(lines[1]["count"], 3);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_response_record_has_status_timing_and_size() {
        let line = render(
            &config(""),
            Event::Response(ResponseEvent {
                id: "abc".to_owned(),
                at: UNIX_EPOCH,
                status: Some(200),
                duration_ms: 42,
                bytes: 1234,
            }),
        );
        let record: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(record["event"], "response");
        assert_eq!(record["status"], 200);
        assert_eq!(record["duration_ms"], 42);
        assert_eq!(record["bytes"], 1234);
    }
}
