//! Renders log events in the Open Cybersecurity Schema Framework (OCSF 1.3), so a SIEM can read
//! them without a custom parser.
//!
//! A request and its response are two HTTP Activity events (class 4002) that share
//! `metadata.correlation_uid`. A detection plugin's finding is a Detection Finding (class 2004)
//! with the same correlation id. Values go through the same redaction as the JSON format, and a
//! finding names a kind and a count, never the matched text.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::redact;
use crate::writer::{FindingEvent, RequestEvent, ResponseEvent};

const VERSION: &str = "1.3.0";

fn millis(at: SystemTime) -> u64 {
    u64::try_from(
        at.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(0)
}

fn metadata(id: &str) -> Value {
    json!({
        "version": VERSION,
        "product": {"name": "Reiki", "vendor_name": "Reiki"},
        "correlation_uid": id,
    })
}

/// OCSF HTTP Activity `activity_id` and name for a method.
fn activity(method: &str) -> (u32, &'static str) {
    match method.to_ascii_uppercase().as_str() {
        "CONNECT" => (1, "Connect"),
        "DELETE" => (2, "Delete"),
        "GET" => (3, "Get"),
        "HEAD" => (4, "Head"),
        "OPTIONS" => (5, "Options"),
        "POST" => (6, "Post"),
        "PUT" => (7, "Put"),
        "TRACE" => (8, "Trace"),
        _ => (99, "Other"),
    }
}

fn http_activity(id: &str, at: SystemTime, activity_id: u32, name: &str) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("class_uid".into(), json!(4002));
    event.insert("class_name".into(), json!("HTTP Activity"));
    event.insert("category_uid".into(), json!(4));
    event.insert("category_name".into(), json!("Network Activity"));
    event.insert("activity_id".into(), json!(activity_id));
    event.insert("activity_name".into(), json!(name));
    event.insert("type_uid".into(), json!(400_200 + activity_id));
    event.insert("severity_id".into(), json!(1));
    event.insert("time".into(), json!(millis(at)));
    event.insert("metadata".into(), metadata(id));
    event
}

pub fn request(config: &Config, request: RequestEvent) -> String {
    let (activity_id, name) = activity(&request.method);
    let mut event = http_activity(&request.id, request.at, activity_id, name);
    event.insert("status_id".into(), json!(0));
    let path = if config.redact {
        redact::path(&request.path)
    } else {
        request.path.clone()
    };
    let mut http_request = Map::new();
    http_request.insert("http_method".into(), json!(request.method));
    http_request.insert("url".into(), json!({"path": path}));
    http_request.insert("length".into(), json!(request.body_bytes));
    let headers: Vec<Value> = request
        .headers
        .iter()
        .map(|(name, value)| {
            let value = if config.redact {
                redact::header(name, value).into_owned()
            } else {
                value.clone()
            };
            if name.eq_ignore_ascii_case("user-agent") {
                http_request.insert("user_agent".into(), json!(value));
            }
            json!({"name": name, "value": value})
        })
        .collect();
    if !headers.is_empty() {
        http_request.insert("http_headers".into(), Value::Array(headers));
    }
    event.insert("http_request".into(), Value::Object(http_request));
    event.insert("src_endpoint".into(), json!({"name": request.stream}));
    event.insert("dst_endpoint".into(), json!({"name": request.upstream}));
    Value::Object(event).to_string()
}

pub fn response(response: ResponseEvent) -> String {
    let mut event = http_activity(&response.id, response.at, 0, "Unknown");
    event.insert("type_uid".into(), json!(400_200));
    event.insert("duration".into(), json!(response.duration_ms));
    match response.status {
        Some(code) => {
            let failed = code >= 400;
            event.insert("status_id".into(), json!(if failed { 2 } else { 1 }));
            event.insert("status_code".into(), json!(code.to_string()));
            event.insert(
                "http_response".into(),
                json!({"code": code, "length": response.bytes}),
            );
            if failed {
                event.insert("severity_id".into(), json!(2));
            }
        }
        None => {
            event.insert("status_id".into(), json!(2));
            event.insert("status_detail".into(), json!("no response"));
        }
    }
    if let Some(key) = response.key {
        event.insert("unmapped".into(), json!({"key": key}));
    }
    Value::Object(event).to_string()
}

pub fn finding(finding: FindingEvent) -> String {
    let (disposition_id, disposition, severity_id) = if finding.blocked {
        (2, "Blocked", 4)
    } else {
        (17, "Logged", 3)
    };
    json!({
        "class_uid": 2004,
        "class_name": "Detection Finding",
        "category_uid": 2,
        "category_name": "Findings",
        "activity_id": 1,
        "activity_name": "Create",
        "type_uid": 200_401,
        "severity_id": severity_id,
        "status_id": 1,
        "time": millis(finding.at),
        "message": format!("{}: {} x{}", finding.plugin, finding.kind, finding.count),
        "count": finding.count,
        "disposition_id": disposition_id,
        "disposition": disposition,
        "finding_info": {
            "title": format!("{}: {} in an outbound request", finding.plugin, finding.kind),
            "uid": format!("{}:{}:{}", finding.id, finding.plugin, finding.kind),
            "types": [finding.kind],
        },
        "src_endpoint": {"name": finding.stream},
        "metadata": metadata(&finding.id),
        "unmapped": {"plugin": finding.plugin},
    })
    .to_string()
}
