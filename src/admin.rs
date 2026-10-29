use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;

use crate::auth::{constant_time_eq, sha256_hex};
use crate::chain::{KeyState, KeyStatus};
use crate::config::StrategyConfig;
use crate::proxy::{App, Snapshot};
use reiki_plugin::KeyCounts;

/// The admin listener's routes. Mounted on its own socket, never on the client
/// listener, so it is only reachable where the operator exposes it.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/reload", post(reload))
        .fallback(get(plugin_route))
        .with_state(app)
}

async fn status(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !access_allowed(&app, &headers) {
        return unauthorized();
    }
    let snapshot = app.snapshot();
    let now = Instant::now();
    let body = {
        let health = app
            .health()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        build_status(&snapshot, &health, now)
    };
    let body = {
        let mut body = body;
        body.pop();
        format!("{body},\"config\":{}}}", app.reload_status_json())
    };
    let body = match app.ca_not_after() {
        Some(not_after) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
            let days_left = (not_after - now).div_euclid(86_400);
            let mut body = body;
            body.pop();
            format!("{body},\"intercept\":{{\"ca_not_after\":{not_after},\"ca_days_left\":{days_left}}}}}")
        }
        None => body,
    };
    text_response(StatusCode::OK, "application/json", body)
}

/// Re-reads the config and applies what can change live, like SIGHUP, and returns what changed.
async fn reload(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap) -> Response {
    if !access_allowed(&app, &headers) {
        return unauthorized();
    }
    let mut dry_run = false;
    for pair in uri.query().unwrap_or("").split('&') {
        match pair {
            "" => {}
            "dry_run" | "dry_run=1" | "dry_run=true" => dry_run = true,
            "dry_run=0" | "dry_run=false" => {}
            other if other.starts_with("dry_run=") => {
                // Fail closed: an unreadable preview flag must not apply a live reload.
                return text_response(
                    StatusCode::BAD_REQUEST,
                    "application/json",
                    "{\"ok\":false,\"error\":\"dry_run must be true, 1, false or 0\"}".to_owned(),
                );
            }
            _ => {}
        }
    }
    let worker = Arc::clone(&app);
    let outcome = tokio::task::spawn_blocking(move || {
        if dry_run {
            worker.preview_reload()
        } else {
            worker.reload_now()
        }
    })
    .await;
    match outcome {
        Ok(Ok(report)) => text_response(StatusCode::OK, "application/json", report.to_json()),
        Ok(Err(error)) => text_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "application/json",
            crate::reload::error_json(&error),
        ),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            crate::reload::error_json("the reload task failed"),
        ),
    }
}

async fn plugin_route(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap) -> Response {
    if !access_allowed(&app, &headers) {
        return unauthorized();
    }
    let snapshot = app.snapshot();
    let now = Instant::now();
    let keys = {
        let health = app
            .health()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut counts = KeyCounts::default();
        for name in snapshot.config.keys.keys() {
            match health.status(name, now) {
                KeyStatus::Available => counts.available += 1,
                KeyStatus::Cooling(_) => counts.cooling += 1,
                KeyStatus::OutOfRotation => counts.out_of_rotation += 1,
            }
        }
        counts
    };
    match app.plugins().on_admin(uri.path(), keys) {
        Some(reply) => text_response(StatusCode::OK, reply.content_type, reply.body),
        None => text_response(StatusCode::NOT_FOUND, "text/plain", String::new()),
    }
}

/// Constant-time check of the `Authorization: Bearer <token>` header against the
/// configured admin token hash. A missing config or header is a plain refusal.
fn access_allowed(app: &App, headers: &HeaderMap) -> bool {
    let snapshot = app.snapshot();
    let Some(admin) = snapshot.config.admin.as_ref() else {
        return false;
    };
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = bearer_token(value) else {
        return false;
    };
    let hash = sha256_hex(token);
    constant_time_eq(&admin.token_sha256.to_ascii_lowercase(), &hash)
}

/// The credential of an `Authorization: Bearer <token>` value. The scheme name is
/// case-insensitive.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

fn unauthorized() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}

fn text_response(status: StatusCode, content_type: &'static str, body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// Snapshot of key health and stream routing for operators. Carries no key
/// material, token hashes or file paths.
fn build_status(snapshot: &Snapshot, health: &KeyState, now: Instant) -> String {
    let mut out = String::from("{\"keys\":[");
    for (index, (name, key)) in snapshot.config.keys.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        out.push_str(&json_string(name));
        out.push_str(",\"upstream\":");
        out.push_str(&json_string(&key.upstream));
        out.push_str(",\"consecutive_failures\":");
        out.push_str(&health.consecutive_failures(name).to_string());
        match health.status(name, now) {
            KeyStatus::Available => out.push_str(",\"state\":\"available\""),
            KeyStatus::OutOfRotation => out.push_str(",\"state\":\"out_of_rotation\""),
            KeyStatus::Cooling(remaining) => {
                out.push_str(",\"state\":\"cooling\",\"retry_after_seconds\":");
                out.push_str(&remaining.as_secs().max(1).to_string());
            }
        }
        out.push('}');
    }

    out.push_str("],\"streams\":[");
    for (index, (name, stream)) in snapshot.config.streams.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let current = snapshot.chains.get(name).and_then(|chain| {
            chain
                .lock()
                .ok()
                .and_then(|guard| guard.current().map(str::to_owned))
        });
        out.push_str("{\"name\":");
        out.push_str(&json_string(name));
        out.push_str(",\"strategy\":\"");
        out.push_str(match stream.strategy {
            StrategyConfig::Sticky => "sticky",
            StrategyConfig::RoundRobin => "round_robin",
        });
        out.push_str("\",\"chain\":[");
        for (chain_index, key) in stream.chain.iter().enumerate() {
            if chain_index > 0 {
                out.push(',');
            }
            out.push_str(&json_string(key));
        }
        out.push_str("],\"current_key\":");
        match &current {
            Some(key) => out.push_str(&json_string(key)),
            None => out.push_str("null"),
        }
        out.push_str(",\"rate_limit_per_minute\":");
        match stream.rate_limit_per_minute {
            Some(limit) => out.push_str(&limit.to_string()),
            None => out.push_str("null"),
        }
        out.push('}');
    }
    out.push_str("]}");
    out
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_scheme_is_case_insensitive_but_the_token_is_exact() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer Abc"), Some("Abc"));
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearerabc"), None);
    }

    #[test]
    fn json_string_escapes_control_characters() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }
}
