use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::config::{BodyMode, Config, LogFormat, SseMode};
use crate::redact;
use crate::shape;
use crate::sink::FileSink;
#[cfg(unix)]
use crate::sink::SocketHub;
use crate::timefmt::rfc3339_millis;

const MAX_HEADER_VALUE: usize = 256;

pub enum Event {
    Request(RequestEvent),
    Response(ResponseEvent),
    Finding(FindingEvent),
    Shutdown,
}

/// A detection plugin found identifiers in a request. It names a kind and a count, never the text.
pub struct FindingEvent {
    pub id: String,
    pub at: SystemTime,
    pub stream: String,
    pub plugin: String,
    pub kind: String,
    pub count: u64,
    pub blocked: bool,
}

pub struct RequestEvent {
    pub id: String,
    pub at: SystemTime,
    pub stream: String,
    pub upstream: String,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body_bytes: usize,
    pub body: Option<Bytes>,
}

/// One upstream attempt: the key's name in the config and the status it returned.
pub struct AttemptEvent {
    pub key: String,
    pub status: Option<u16>,
    /// The upstream's error body, only in `response = "body"` mode.
    pub body: Option<Vec<u8>>,
}

pub struct ResponseEvent {
    pub id: String,
    pub at: SystemTime,
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub bytes: u64,
    /// The key that produced the response, `None` when Reiki answered itself.
    pub key: Option<String>,
    pub attempts: Vec<AttemptEvent>,
    /// The leading part of the response body, only in `response = "body"` mode.
    pub body: Option<Vec<u8>>,
    pub body_truncated: bool,
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
    if config.format == LogFormat::Ocsf {
        return match event {
            Event::Request(request) => crate::ocsf::request(config, request),
            Event::Response(response) => crate::ocsf::response(response),
            Event::Finding(finding) => crate::ocsf::finding(finding),
            Event::Shutdown => String::new(),
        };
    }
    match event {
        Event::Request(request) => render_request(config, request),
        Event::Shutdown => String::new(),
        Event::Response(response) => render_response(config, response),
        Event::Finding(finding) => render_finding(finding),
    }
}

fn render_finding(finding: FindingEvent) -> String {
    json!({
        "v": 1,
        "event": "finding",
        "id": finding.id,
        "ts": rfc3339_millis(finding.at),
        "stream": finding.stream,
        "plugin": finding.plugin,
        "kind": finding.kind,
        "count": finding.count,
        "action": if finding.blocked { "blocked" } else { "reported" },
    })
    .to_string()
}

fn render_response(config: &Config, response: ResponseEvent) -> String {
    let mut record = Map::new();
    record.insert("v".into(), json!(1));
    record.insert("event".into(), json!("response"));
    record.insert("id".into(), json!(response.id));
    record.insert("ts".into(), json!(rfc3339_millis(response.at)));
    record.insert("status".into(), json!(response.status));
    record.insert("duration_ms".into(), json!(response.duration_ms));
    record.insert("bytes".into(), json!(response.bytes));
    record.insert("key".into(), json!(response.key));
    if !response.attempts.is_empty() {
        let attempts: Vec<Value> = response
            .attempts
            .iter()
            .map(|a| {
                let mut entry = Map::new();
                entry.insert("key".into(), json!(a.key));
                entry.insert("status".into(), json!(a.status));
                if let Some(body) = a.body.as_deref().filter(|b| !b.is_empty()) {
                    entry.insert("body".into(), full_body(config, body).value);
                }
                Value::Object(entry)
            })
            .collect();
        record.insert("attempts".into(), Value::Array(attempts));
    }
    if let Some(body) = response.body.filter(|b| !b.is_empty()) {
        if let Some(events) = sse_body(config, &body, response.body_truncated) {
            record.insert("body".into(), events);
            record.insert("body_format".into(), json!("sse"));
            if response.body_truncated {
                record.insert("body_truncated".into(), json!(true));
            }
        } else {
            let shaped = full_body(config, &body);
            record.insert("body".into(), shaped.value);
            if !shaped.excluded.is_empty() {
                record.insert("body_excluded".into(), json!(shaped.excluded));
            }
            if shaped.truncated || response.body_truncated {
                record.insert("body_truncated".into(), json!(true));
            }
        }
    }
    Value::Object(record).to_string()
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
    let path = if config.redact {
        redact::path(&request.path)
    } else {
        request.path
    };
    record.insert("path".into(), json!(path));

    if !request.headers.is_empty() {
        let headers: Map<String, Value> = request
            .headers
            .iter()
            .map(|(name, value)| {
                let value = if config.redact {
                    redact::header(name, value).into_owned()
                } else {
                    value.clone()
                };
                let value = truncate(&value, MAX_HEADER_VALUE);
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
                let shaped = full_body(config, &body);
                record.insert("body".into(), shaped.value);
                if !shaped.excluded.is_empty() {
                    record.insert("body_excluded".into(), json!(shaped.excluded));
                }
                if shaped.truncated {
                    record.insert("body_truncated".into(), json!(true));
                }
            }
        }
    }
    Value::Object(record).to_string()
}

/// A body ready to log: parsed and trimmed when it is JSON, masked text otherwise.
struct Shaped {
    value: Value,
    truncated: bool,
    /// Paths dropped by `exclude`.
    excluded: Vec<String>,
}

fn full_body(config: &Config, body: &[u8]) -> Shaped {
    let masked = |text: &str| {
        if config.redact {
            redact::secrets(text).into_owned()
        } else {
            text.to_owned()
        }
    };
    match serde_json::from_slice::<Value>(body) {
        Ok(mut value) => {
            let excluded = shape::apply(&mut value, &config.exclude, &config.summarize);
            if config.redact {
                redact::json(&mut value);
            }
            if config.max_string_bytes > 0 {
                shape::truncate_strings(&mut value, config.max_string_bytes);
            }
            let text = value.to_string();
            if text.len() <= config.max_body_bytes {
                Shaped {
                    value,
                    truncated: false,
                    excluded,
                }
            } else {
                Shaped {
                    value: Value::String(truncate(&text, config.max_body_bytes)),
                    truncated: true,
                    excluded,
                }
            }
        }
        Err(_) => {
            let text = masked(&String::from_utf8_lossy(body));
            Shaped {
                truncated: text.len() > config.max_body_bytes,
                value: Value::String(truncate(&text, config.max_body_bytes)),
                excluded: Vec::new(),
            }
        }
    }
}

/// An event-stream response as parsed events or one assembled completion; `None` when the body
/// is not an event stream or the log is set to keep it raw.
fn sse_body(config: &Config, body: &[u8], cut: bool) -> Option<Value> {
    if config.sse == SseMode::Raw || serde_json::from_slice::<Value>(body).is_ok() {
        return None;
    }
    let mut text = String::from_utf8_lossy(body).into_owned();
    if cut {
        // The capture stopped mid-line, so the last event is incomplete.
        text.truncate(text.rfind('\n').map_or(0, |i| i + 1));
    }
    let events = shape::sse_events(&text)?;
    let mut value = match config.sse {
        SseMode::Assembled => shape::assemble(&events),
        _ => Value::Array(events),
    };
    if config.redact {
        redact::json(&mut value);
    }
    if config.max_string_bytes > 0 {
        shape::truncate_strings(&mut value, config.max_string_bytes);
    }
    Some(value)
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
            path: "/zen/chat/completions".to_owned(),
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
                key: Some("zen-primary".to_owned()),
                attempts: vec![
                    AttemptEvent {
                        key: "zen-first".to_owned(),
                        status: Some(403),
                        body: Some(br#"{"error":"refused"}"#.to_vec()),
                    },
                    AttemptEvent {
                        key: "zen-primary".to_owned(),
                        status: Some(200),
                        body: None,
                    },
                ],
                body: None,
                body_truncated: false,
            }),
        );
        let record: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(record["event"], "response");
        assert_eq!(record["status"], 200);
        assert_eq!(record["duration_ms"], 42);
        assert_eq!(record["bytes"], 1234);
        assert_eq!(record["key"], "zen-primary");
        assert_eq!(record["attempts"][0]["key"], "zen-first");
        assert_eq!(record["attempts"][0]["status"], 403);
        assert_eq!(record["attempts"][0]["body"]["error"], "refused");
        assert!(record["attempts"][1].get("body").is_none());
        assert!(record.get("body").is_none());
    }

    fn response_with(body: Option<&[u8]>, truncated: bool) -> ResponseEvent {
        ResponseEvent {
            id: "abc".to_owned(),
            at: UNIX_EPOCH,
            status: Some(200),
            duration_ms: 1,
            bytes: 10,
            key: Some("k".to_owned()),
            attempts: Vec::new(),
            body: body.map(<[u8]>::to_vec),
            body_truncated: truncated,
        }
    }

    #[test]
    fn a_response_body_is_parsed_masked_and_marked_when_cut() {
        let json = br#"{"answer":"ok","note":"Bearer abcdefghijklmnopqrstuvwxyz0123456789"}"#;
        let record: Value = serde_json::from_str(&render(
            &config(""),
            Event::Response(response_with(Some(json), false)),
        ))
        .unwrap();
        assert_eq!(record["body"]["answer"], "ok");
        assert!(!record
            .to_string()
            .contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(record.get("body_truncated").is_none());

        let sse = b"data: {\"a\":1}\n\ndata: [DONE]\n\n";
        let record: Value = serde_json::from_str(&render(
            &config("sse = \"raw\""),
            Event::Response(response_with(Some(sse), true)),
        ))
        .unwrap();
        assert!(record["body"].as_str().unwrap().contains("[DONE]"));
        assert_eq!(record["body_truncated"], true);

        let record: Value = serde_json::from_str(&render(
            &config(""),
            Event::Response(response_with(Some(b""), false)),
        ))
        .unwrap();
        assert!(record.get("body").is_none());
    }

    #[test]
    fn excluded_and_summarized_paths_leave_a_smaller_json_body() {
        let body = r#"{"model":"m","tools":[{"function":{"name":"bash"}}],"system":"abcdefghij"}"#;
        let config = config("exclude = [\"system\"]\nsummarize = [\"tools\"]");
        let record = parsed(&config, request(body));
        assert_eq!(record["body"], json!({"model": "m", "tools": ["bash"]}));
        assert_eq!(record["body_excluded"], json!(["system"]));
        assert_eq!(record["body_bytes"], body.len());
        assert!(record.get("body_truncated").is_none());
    }

    #[test]
    fn long_strings_are_cut_before_the_size_cap_so_the_body_stays_json() {
        let long = "x".repeat(5000);
        let body = format!(r#"{{"a":"{long}","b":"{long}"}}"#);
        let config = config("max_body_bytes = 2000\nmax_string_bytes = 100");
        let record = parsed(&config, request(&body));
        assert!(record["body"].is_object(), "{record}");
        assert!(record["body"]["a"]
            .as_str()
            .unwrap()
            .contains("…[+4900 bytes]"));
        assert!(record.get("body_truncated").is_none());
    }

    #[test]
    fn an_event_stream_is_logged_as_parsed_events() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let record: Value = serde_json::from_str(&render(
            &config(""),
            Event::Response(response_with(Some(sse), false)),
        ))
        .unwrap();
        assert_eq!(record["body_format"], "sse");
        assert_eq!(record["body"][0]["choices"][0]["delta"]["content"], "hi");
        assert_eq!(record["body"][1], "[DONE]");
    }

    #[test]
    fn an_assembled_stream_keeps_text_and_drops_a_cut_final_event() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"po\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"con";
        let record: Value = serde_json::from_str(&render(
            &config("sse = \"assembled\""),
            Event::Response(response_with(Some(sse), true)),
        ))
        .unwrap();
        assert_eq!(record["body"]["text"], "po");
        assert_eq!(record["body"]["events"], 1);
        assert_eq!(record["body_truncated"], true);
    }

    fn finding(blocked: bool) -> FindingEvent {
        FindingEvent {
            id: "abc".to_owned(),
            at: UNIX_EPOCH + Duration::from_secs(951_782_401),
            stream: "zen".to_owned(),
            plugin: "phi".to_owned(),
            kind: "ssn".to_owned(),
            count: 2,
            blocked,
        }
    }

    fn response() -> ResponseEvent {
        ResponseEvent {
            id: "abc".to_owned(),
            at: UNIX_EPOCH + Duration::from_secs(951_782_402),
            status: Some(429),
            duration_ms: 120,
            bytes: 64,
            key: Some("zen-a".to_owned()),
            attempts: Vec::new(),
            body: None,
            body_truncated: false,
        }
    }

    fn line(config: &Config, event: Event) -> Value {
        serde_json::from_str(&render(config, event)).unwrap()
    }

    #[test]
    fn the_json_format_records_a_finding_with_its_request_id() {
        let c = config("");
        let record = line(&c, Event::Finding(finding(true)));
        assert_eq!(record["event"], "finding");
        assert_eq!(record["id"], "abc");
        assert_eq!(
            (
                record["plugin"].as_str(),
                record["kind"].as_str(),
                record["count"].as_u64()
            ),
            (Some("phi"), Some("ssn"), Some(2))
        );
        assert_eq!(record["action"], "blocked");
        assert_eq!(
            line(&c, Event::Finding(finding(false)))["action"],
            "reported"
        );
    }

    #[test]
    fn ocsf_requests_are_http_activity_events() {
        let c = config("format = \"ocsf\"\nheaders = [\"user-agent\"]");
        let event = line(&c, Event::Request(request("{}")));
        assert_eq!(
            (event["class_uid"].as_u64(), event["type_uid"].as_u64()),
            (Some(4002), Some(400_206))
        );
        assert_eq!(event["activity_name"], "Post");
        let mut patch = request("{}");
        patch.method = "PATCH".to_owned();
        let patched = line(&c, Event::Request(patch));
        assert_eq!(
            (
                patched["activity_id"].as_u64(),
                patched["type_uid"].as_u64()
            ),
            (Some(9), Some(400_209))
        );
        assert_eq!(event["time"], 951_782_400_000_u64);
        assert_eq!(event["metadata"]["correlation_uid"], "abc");
        assert_eq!(event["metadata"]["version"], "1.3.0");
        assert_eq!(event["http_request"]["http_method"], "POST");
        assert_eq!(
            event["http_request"]["url"]["path"],
            "/zen/chat/completions"
        );
        assert_eq!(event["http_request"]["user_agent"], "opencode/1.18.35");
        assert_eq!(event["src_endpoint"]["name"], "zen");
    }

    #[test]
    fn ocsf_responses_carry_the_status_and_share_the_request_id() {
        let c = config("format = \"ocsf\"");
        let event = line(&c, Event::Response(response()));
        assert_eq!(event["class_uid"], 4002);
        assert_eq!(event["metadata"]["correlation_uid"], "abc");
        assert_eq!(event["http_response"]["code"], 429);
        assert_eq!(
            (event["status_id"].as_u64(), event["duration"].as_u64()),
            (Some(2), Some(120))
        );
        let mut ok = response();
        ok.status = Some(200);
        assert_eq!(line(&c, Event::Response(ok))["status_id"], 1);
        let mut none = response();
        none.status = None;
        assert_eq!(
            line(&c, Event::Response(none))["status_detail"],
            "no response"
        );
    }

    #[test]
    fn ocsf_findings_are_detection_findings_that_name_a_kind_and_a_count() {
        let c = config("format = \"ocsf\"");
        let blocked = line(&c, Event::Finding(finding(true)));
        assert_eq!(
            (blocked["class_uid"].as_u64(), blocked["type_uid"].as_u64()),
            (Some(2004), Some(200_401))
        );
        assert_eq!(blocked["disposition"], "Blocked");
        assert_eq!(blocked["finding_info"]["types"][0], "ssn");
        assert_eq!(blocked["finding_info"]["created_time"], 951_782_401_000_u64);
        assert_eq!(blocked["count"], 2);
        assert_eq!(blocked["metadata"]["correlation_uid"], "abc");
        let logged = line(&c, Event::Finding(finding(false)));
        assert_eq!(
            (
                logged["disposition"].as_str(),
                logged["severity_id"].as_u64()
            ),
            (Some("Logged"), Some(3))
        );
    }

    #[test]
    fn ocsf_output_masks_credentials_in_the_path_and_headers() {
        let c = config("format = \"ocsf\"\nheaders = [\"user-agent\"]");
        let mut event = request("{}");
        event.path = "/zen/models?key=hunter2".to_owned();
        event.headers = vec![(
            "user-agent".to_owned(),
            "tool Bearer abcdef0123456789abcdef".to_owned(),
        )];
        let text = render(&c, Event::Request(event));
        assert!(
            !text.contains("hunter2") && !text.contains("abcdef0123456789abcdef"),
            "{text}"
        );
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[test]
    fn the_format_setting_is_checked() {
        assert_eq!(config("").format, LogFormat::Json);
        assert_eq!(config("format = \"ocsf\"").format, LogFormat::Ocsf);
        let bad: toml::Table = "file = \"/x\"\nformat = \"xml\"".parse().unwrap();
        assert!(Config::parse(&bad).unwrap_err().contains("format"));
    }
}
