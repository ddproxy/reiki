use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post, MethodRouter};
use axum::Router;

use crate::auth::{constant_time_eq, sha256_hex, Peer};
use crate::chain::{KeyState, KeyStatus};
use crate::config::StrategyConfig;
use crate::proxy::{App, Snapshot};
use reiki_plugin::KeyCounts;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prefix of the versioned copy of every admin route. v1 only gains fields.
const V1: &str = "/v1";

/// The routes this file serves itself. Each is mounted bare and under [`V1`], and the OpenAPI
/// document is built from this table, so the two cannot drift.
const ROUTES: &[(&str, &str, &str)] = &[
    ("/status", "get", "Key, stream and config status"),
    (
        "/reload",
        "post",
        "Reload the config, or preview it with dry_run",
    ),
    ("/keys/enable", "post", "Re-enable a disabled key"),
];

fn handler(path: &str, method: &str) -> MethodRouter<Arc<App>> {
    match (path, method) {
        ("/reload", "post") => post(reload),
        ("/keys/enable", "post") => post(enable_key),
        _ => get(status),
    }
}

/// The admin listener's routes. Mounted on its own socket, never on the client
/// listener, so it is only reachable where the operator exposes it.
pub fn router(app: Arc<App>) -> Router {
    let mut router = Router::new();
    for (path, method, _) in ROUTES {
        let route = handler(path, method);
        router = router
            .route(path, route.clone())
            .route(&format!("{V1}{path}"), route);
    }
    router
        .route(&format!("{V1}/openapi.json"), get(openapi))
        .fallback(get(plugin_route))
        .layer(axum::middleware::map_response(with_version))
        .with_state(app)
}

async fn with_version(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("x-reiki-version", HeaderValue::from_static(VERSION));
    response
}

fn operation(summary: &str) -> serde_json::Value {
    serde_json::json!({
        "summary": summary,
        "security": [{"bearerAuth": []}],
        "responses": {"200": {"description": "OK"}, "401": {"description": "Missing or wrong token"}},
    })
}

/// The OpenAPI 3.1 description of the v1 routes, including those plugins add.
async fn openapi(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, "/v1/openapi.json", &headers) {
        return *refusal;
    }
    let mut paths = serde_json::Map::new();
    for (path, method, summary) in ROUTES {
        let mut op = operation(summary);
        if *path == "/reload" {
            op["parameters"] = serde_json::json!([{
                "name": "dry_run", "in": "query", "required": false,
                "schema": {"type": "string", "enum": ["true", "1", "false", "0"]},
            }]);
            op["responses"]["400"] =
                serde_json::json!({"description": "Unreadable dry_run value; nothing changed"});
            op["responses"]["422"] =
                serde_json::json!({"description": "The config was refused; nothing changed"});
        }
        if *path == "/keys/enable" {
            op["parameters"] = serde_json::json!([{
                "name": "key", "in": "query", "required": true,
                "schema": {"type": "string"},
            }]);
            op["responses"]["400"] = serde_json::json!({
                "description": "Missing key name, or a name that is not valid percent-encoding"
            });
            op["responses"]["404"] = serde_json::json!({"description": "No such key"});
        }
        paths.insert(format!("{V1}{path}"), serde_json::json!({ *method: op }));
    }
    for path in app.plugins().admin_paths() {
        paths.insert(
            format!("{V1}{path}"),
            serde_json::json!({"get": operation("Provided by a plugin")}),
        );
    }
    paths.insert(
        format!("{V1}/openapi.json"),
        serde_json::json!({"get": operation("This document")}),
    );
    #[cfg(feature = "peers")]
    let peer_routes = crate::peers::openapi_paths(&app.snapshot().config);
    #[cfg(feature = "peers")]
    paths.extend(
        peer_routes
            .iter()
            .map(|(p, item)| (p.clone(), item.clone())),
    );
    #[allow(unused_mut)]
    let mut document = serde_json::json!({
        "openapi": "3.1.0",
        "info": {"title": "Reiki admin API", "version": VERSION},
        "components": {"securitySchemes": {"bearerAuth": {"type": "http", "scheme": "bearer"}}},
        "paths": paths,
    });
    #[cfg(feature = "peers")]
    if !peer_routes.is_empty() {
        document["components"]["securitySchemes"]["peerAuth"] = serde_json::json!({
            "type": "http", "scheme": "bearer",
            "description": "The read-only peer token; accepted on the peer listener only",
        });
    }
    text_response(StatusCode::OK, "application/json", document.to_string())
}

async fn status(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, "/status", &headers) {
        return *refusal;
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
async fn reload(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, "/reload", &headers) {
        return *refusal;
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

/// Percent-decodes one query value. `+` is a space; an invalid escape or bytes that are not
/// UTF-8 yields `None`, so the caller answers 400 instead of guessing the key name.
fn decode_query_value(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Re-enables a key that was taken out of rotation, so its chain may pick it again. Sends
/// the key's name as `?key=<name>`; an unknown name answers `404`.
async fn enable_key(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, "/keys/enable", &headers) {
        return *refusal;
    }
    let key = uri
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("key="))
        .filter(|key| !key.is_empty());
    let Some(key) = key else {
        return text_response(
            StatusCode::BAD_REQUEST,
            "application/json",
            "{\"ok\":false,\"error\":\"give the key name as ?key=<name>\"}".to_owned(),
        );
    };
    let key = match decode_query_value(key).filter(|key| !key.is_empty()) {
        Some(key) => key,
        None => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "application/json",
                "{\"ok\":false,\"error\":\"the key name is not valid percent-encoding\"}"
                    .to_owned(),
            );
        }
    };
    if app.enable_key(&key) {
        tracing::info!(key, "key re-enabled through the admin listener");
        text_response(
            StatusCode::OK,
            "application/json",
            format!("{{\"ok\":true,\"key\":{}}}", json_string(&key)),
        )
    } else {
        text_response(
            StatusCode::NOT_FOUND,
            "application/json",
            format!(
                "{{\"ok\":false,\"error\":\"unknown key\",\"key\":{}}}",
                json_string(&key)
            ),
        )
    }
}

async fn plugin_route(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, uri.path(), &headers) {
        return *refusal;
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
    let path = uri.path();
    let path = path
        .strip_prefix(V1)
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(path);
    match app.plugins().on_admin(path, keys) {
        Some(reply) => text_response(StatusCode::OK, reply.content_type, reply.body),
        None => text_response(StatusCode::NOT_FOUND, "text/plain", String::new()),
    }
}

/// Checks a request's bearer token against the configured admin token hash, logging each failure
/// with the peer and path and counting it for the peer's lockout. The presented token is never
/// logged. A missing config or header is a plain refusal.
fn authorize(
    app: &App,
    peer: Option<IpAddr>,
    path: &str,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    if let Some(ip) = peer {
        if let Some(wait) = app.admin_logins().blocked(ip, Instant::now()) {
            tracing::warn!(?peer, path = %path, "too many failed admin logins, refusing");
            return Err(Box::new(throttled(wait)));
        }
    }
    match token_matches(app, headers) {
        Ok(()) => {
            if let Some(ip) = peer {
                app.admin_logins().succeed(ip);
            }
            Ok(())
        }
        Err(reason) => {
            let failures = peer.map(|ip| app.admin_logins().fail(ip, Instant::now()));
            tracing::warn!(?peer, reason, ?failures, path = %path, "failed admin login");
            Err(Box::new(unauthorized()))
        }
    }
}

/// Why a request's credential does not match, or `Ok` when it does.
fn token_matches(app: &App, headers: &HeaderMap) -> Result<(), &'static str> {
    let snapshot = app.snapshot();
    let Some(admin) = snapshot.config.admin.as_ref() else {
        return Err("no admin token configured");
    };
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err("missing authorization header");
    };
    let Ok(value) = value.to_str() else {
        return Err("authorization header is not text");
    };
    let Some(token) = bearer_token(value) else {
        return Err("not a bearer credential");
    };
    let hash = sha256_hex(token);
    if constant_time_eq(&admin.token_sha256.to_ascii_lowercase(), &hash) {
        Ok(())
    } else {
        Err("wrong token")
    }
}

/// The credential of an `Authorization: Bearer <token>` value. The scheme name is
/// case-insensitive.
pub(crate) fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

pub(crate) fn unauthorized() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
}

/// A refused client waiting out a login lockout, with the remaining seconds.
pub(crate) fn throttled(wait: Duration) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    if let Ok(value) = HeaderValue::from_str(&secs.max(1).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

pub(crate) fn text_response(
    status: StatusCode,
    content_type: &'static str,
    body: String,
) -> Response {
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
        out.push_str(",\"last_status_code\":");
        match health.last_status(name) {
            Some(status) => out.push_str(&status.to_string()),
            None => out.push_str("null"),
        }
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
