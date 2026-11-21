use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::Method;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post, MethodRouter};
use axum::Router;
use serde_json::Value;

use crate::auth::{bearer_token, constant_time_eq, sha256_hex, Peer};
use crate::chain::{KeyState, KeyStatus};
use crate::config::StrategyConfig;
use crate::proxy::{App, Snapshot};
use reiki_plugin::KeyCounts;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prefix of every admin route.
const V1: &str = "/v1";

/// The routes this file serves itself, under [`V1`]. The OpenAPI document is built from this
/// table, so the two cannot drift.
const ROUTES: &[(&str, &str, &str)] = &[
    ("/status", "get", "Key, stream and config status"),
    (
        "/reload",
        "post",
        "Reload the config, or preview it with dry_run",
    ),
    ("/keys/enable", "post", "Re-enable a disabled key"),
    (
        "/failures",
        "get",
        "Recent key failures, newest first, from an in-memory ring",
    ),
];

/// True when a plugin path would collide with a route the admin listener serves itself.
///
/// Plugins declare bare paths; the router mounts them under [`V1`], so both forms are checked.
pub(crate) fn is_reserved_path(path: &str) -> bool {
    let bare = path
        .strip_prefix(V1)
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(path);
    bare == "/openapi.json" || ROUTES.iter().any(|(route, _, _)| bare == *route)
}

fn handler(path: &str, method: &str) -> MethodRouter<Arc<App>> {
    match (path, method) {
        ("/reload", "post") => post(reload),
        ("/keys/enable", "post") => post(enable_key),
        ("/failures", "get") => get(failures),
        _ => get(status),
    }
}

/// The admin listener's routes. Mounted on its own socket, never on the client
/// listener, so it is only reachable where the operator exposes it.
pub fn router(app: Arc<App>) -> Router {
    let mut router = Router::new();
    for (path, method, _) in ROUTES {
        router = router.route(&format!("{V1}{path}"), handler(path, method));
    }
    router
        .route(&format!("{V1}/openapi.json"), get(openapi))
        .fallback(plugin_route)
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
        if *path == "/failures" {
            op["parameters"] = serde_json::json!([
                {
                    "name": "limit", "in": "query", "required": false,
                    "schema": {"type": "integer", "minimum": 1, "maximum": crate::failures::CAPACITY},
                    "description": "Records to return, newest first; the default is the whole ring",
                },
                {
                    "name": "key", "in": "query", "required": false,
                    "schema": {"type": "string"},
                    "description": "Only failures for this key name",
                },
            ]);
            op["responses"]["400"] = serde_json::json!({
                "description": "limit is not 1..=64, a flag is repeated, or the key name is empty or invalid percent-encoding"
            });
        }
        if *path == "/keys/enable" {
            op["parameters"] = serde_json::json!([{
                "name": "key", "in": "query", "required": true,
                "schema": {"type": "string"},
            }]);
            op["responses"]["400"] = serde_json::json!({
                "description": "Missing, repeated or unknown flag, or a name that is not valid percent-encoding"
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
        let mut health = app
            .health()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        build_status(&snapshot, &mut health, now)
    };
    let config = app.reload_status_json();
    let intercept = app.ca_not_after().map(|not_after| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let days_left = (not_after - now).div_euclid(86_400);
        format!("{{\"ca_not_after\":{not_after},\"ca_days_left\":{days_left}}}")
    });
    let log = format!(r#"{{"write_errors":{}}}"#, app.file_write_errors());
    let mut fields: Vec<(&str, &str)> = vec![("config", &config)];
    if let Some(intercept) = &intercept {
        fields.push(("intercept", intercept));
    }
    fields.push(("log", &log));
    let Some(body) = merge_status_objects(&body, &fields) else {
        // An unmergeable status must not become a corrupt JSON document on the wire.
        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            r#"{"error":"status_unavailable"}"#.to_owned(),
        );
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
    let Ok(dry_run) = parse_dry_run(uri.query().unwrap_or("")) else {
        // Fail closed: an unreadable preview flag must not apply a live reload.
        return text_response(
            StatusCode::BAD_REQUEST,
            "application/json",
            "{\"ok\":false,\"error\":\"dry_run must be given once, as true, 1, false or 0\"}"
                .to_owned(),
        );
    };
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

/// The `dry_run` flag of a reload query: absent is `false`; a bare name, an empty or unknown
/// value, or a second `dry_run` is an error, so a flag that was not understood never applies a
/// live reload.
pub(crate) fn parse_dry_run(query: &str) -> Result<bool, ()> {
    let mut found = None;
    for pair in query.split('&') {
        let (name, value) = pair
            .split_once('=')
            .map_or((pair, None), |(n, v)| (n, Some(v)));
        if name != "dry_run" {
            continue;
        }
        let parsed = match value {
            Some("true" | "1") => true,
            Some("false" | "0") => false,
            _ => return Err(()),
        };
        if found.replace(parsed).is_some() {
            return Err(());
        }
    }
    Ok(found.unwrap_or(false))
}

/// Percent-decodes one query value. `+` is a space; an invalid escape or bytes that are not
/// UTF-8 yields `None`, so the caller answers 400 instead of guessing the key name.
pub(crate) fn decode_query_value(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                // `from_str_radix` would take a sign, so read the digits by hand.
                let hi = hex_digit(*bytes.get(i + 1)?)?;
                let lo = hex_digit(*bytes.get(i + 2)?)?;
                out.push(hi << 4 | lo);
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

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// Reads `key` strictly: a repeated, bare, empty or unknown flag, or invalid percent-encoding,
/// answers 400 rather than enabling a key the caller did not name unambiguously.
pub(crate) fn parse_key_query(query: &str) -> Result<String, &'static str> {
    let mut key = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let Some((name, value)) = pair.split_once('=') else {
            return Err("every flag needs a value");
        };
        match name {
            "key" => {
                if key.is_some() {
                    return Err("key may be given only once");
                }
                key = Some(
                    decode_query_value(value)
                        .filter(|key| !key.is_empty())
                        .ok_or("the key name is empty or not valid percent-encoding")?,
                );
            }
            _ => return Err("unknown flag; use key"),
        }
    }
    key.ok_or("give the key name as ?key=<name>")
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
    let key = match parse_key_query(uri.query().unwrap_or("")) {
        Ok(key) => key,
        Err(e) => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "application/json",
                format!("{{\"ok\":false,\"error\":{}}}", json_string(e)),
            );
        }
    };
    // Persisting the state writes and syncs a file, which must not stall the runtime.
    let enabled = {
        let (worker, name) = (Arc::clone(&app), key.clone());
        tokio::task::spawn_blocking(move || worker.enable_key(&name)).await
    };
    let Ok(enabled) = enabled else {
        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            "{\"ok\":false,\"error\":\"the enable task failed\"}".to_owned(),
        );
    };
    if enabled {
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

/// The last key failures, newest first. Bodies are snippets already scrubbed and bounded by the
/// request path, so this answer cannot grow with the upstream response.
async fn failures(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, "/failures", &headers) {
        return *refusal;
    }
    let (limit, key) = match parse_failures_query(uri.query().unwrap_or("")) {
        Ok(query) => query,
        Err(e) => {
            return text_response(
                StatusCode::BAD_REQUEST,
                "application/json",
                format!("{{\"ok\":false,\"error\":{}}}", json_string(e)),
            );
        }
    };
    let records = app.failures().snapshot(limit, key.as_deref());
    match serde_json::to_string(&records) {
        Ok(body) => text_response(StatusCode::OK, "application/json", body),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            "{\"ok\":false,\"error\":\"cannot write the failure ring\"}".to_owned(),
        ),
    }
}

/// Reads `limit` and `key` strictly: a repeated or unknown flag, a bare `limit`, an empty key or
/// invalid percent-encoding answers 400 rather than returning a list that was not asked for.
fn parse_failures_query(query: &str) -> Result<(usize, Option<String>), &'static str> {
    let mut limit = crate::failures::CAPACITY;
    let mut key: Option<String> = None;
    let mut saw_limit = false;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let Some((name, value)) = pair.split_once('=') else {
            return Err("every flag needs a value");
        };
        match name {
            "limit" => {
                if saw_limit {
                    return Err("limit may be given only once");
                }
                saw_limit = true;
                let parsed = value
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| (1..=crate::failures::CAPACITY).contains(limit))
                    .ok_or("limit must be a whole number from 1 to 64")?;
                limit = parsed;
            }
            "key" => {
                if key.is_some() {
                    return Err("key may be given only once");
                }
                key = Some(
                    decode_query_value(value)
                        .filter(|key| !key.is_empty())
                        .ok_or("the key name is empty or not valid percent-encoding")?,
                );
            }
            _ => return Err("unknown flag; use limit and key"),
        }
    }
    Ok((limit, key))
}

async fn plugin_route(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    // Plugins answer GET only. Other methods on an unknown path are still a 404, not a 405 from
    // a GET-only fallback.
    if method != Method::GET {
        return text_response(StatusCode::NOT_FOUND, "text/plain", String::new());
    }
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
    let Some(path) = uri
        .path()
        .strip_prefix(V1)
        .filter(|rest| rest.starts_with('/'))
    else {
        return text_response(StatusCode::NOT_FOUND, "text/plain", String::new());
    };
    let usage = snapshot
        .config
        .exposes_usage()
        .then(|| crate::pressure::upstream_usage(&snapshot.config, app.pressure(), now));
    match app.plugins().on_admin(path, keys, usage) {
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
    // Check and reserve in one lock, so a burst cannot all pass before any attempt is counted.
    let attempt = peer.map(|ip| app.admin_logins().reserve(ip, Instant::now()));
    if let Some(Err(wait)) = attempt {
        tracing::warn!(?peer, path = %path, "too many failed admin logins, refusing");
        return Err(Box::new(throttled(wait)));
    }
    match token_matches(app, headers) {
        Ok(()) => {
            if let Some(ip) = peer {
                app.admin_logins().succeed(ip);
            }
            Ok(())
        }
        Err(reason) => {
            let failures = attempt.and_then(Result::ok).map(|a| a.failures());
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

/// Merges JSON object fragments into a status document without mutating its text. Every fragment
/// is parsed before it is appended, so an invalid fragment fails closed instead of producing a
/// corrupt document. The original text is kept verbatim to preserve the status field order.
fn merge_status_objects(body: &str, fields: &[(&str, &str)]) -> Option<String> {
    let document: Value = serde_json::from_str(body).ok()?;
    document.as_object()?;
    let base = body.trim_end().strip_suffix('}')?;
    let mut out =
        String::with_capacity(body.len() + fields.iter().map(|(_, f)| f.len()).sum::<usize>());
    out.push_str(base);
    for (name, fragment) in fields {
        let value: Value = serde_json::from_str(fragment).ok()?;
        if !value.is_object() {
            return None;
        }
        out.push(',');
        out.push_str(&json_string(name));
        out.push(':');
        out.push_str(fragment.trim());
    }
    out.push('}');
    Some(out)
}

/// Snapshot of key health and stream routing for operators. Carries no key
/// material, token hashes or file paths.
fn build_status(snapshot: &Snapshot, health: &mut KeyState, now: Instant) -> String {
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
                if let Some(class) = health.cooling_class(name, now) {
                    out.push_str(",\"cooldown_class\":\"");
                    out.push_str(class.name());
                    out.push('"');
                }
            }
        }
        // Soonest first; always present, including an empty list for a healthy key.
        let scopes = health.cooling_scopes(name, now);
        out.push_str(",\"scopes\":[");
        for (index, cooling) in scopes.iter().take(16).enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"route\":");
            out.push_str(&json_string(&cooling.scope.route));
            out.push_str(",\"model\":");
            out.push_str(&json_string(&cooling.scope.model));
            out.push_str(",\"retry_after_seconds\":");
            let secs =
                cooling.remaining.as_secs() + u64::from(cooling.remaining.subsec_nanos() > 0);
            out.push_str(&secs.max(1).to_string());
            out.push_str(",\"cooldown_class\":\"");
            out.push_str(cooling.class.name());
            out.push_str("\"}");
        }
        out.push(']');
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
    fn percent_decoding_takes_hex_digits_only() {
        assert_eq!(decode_query_value("a%2Fb").as_deref(), Some("a/b"));
        assert_eq!(decode_query_value("a%2fb+c").as_deref(), Some("a/b c"));
        assert_eq!(decode_query_value("%41").as_deref(), Some("A"));
        assert_eq!(decode_query_value("%+1"), None);
        assert_eq!(decode_query_value("%1+"), None);
        assert_eq!(decode_query_value("%-1"), None);
        assert_eq!(decode_query_value("%4"), None);
        assert_eq!(decode_query_value("%"), None);
        assert_eq!(decode_query_value("%zz"), None);
    }

    #[test]
    fn dry_run_is_read_strictly() {
        assert_eq!(parse_dry_run(""), Ok(false));
        assert_eq!(parse_dry_run("other=1"), Ok(false));
        assert_eq!(parse_dry_run("dry_run=true"), Ok(true));
        assert_eq!(parse_dry_run("a=b&dry_run=1&c"), Ok(true));
        assert_eq!(parse_dry_run("dry_run=false"), Ok(false));
        assert_eq!(parse_dry_run("dry_run=0"), Ok(false));
        for bad in [
            "dry_run",
            "dry_run=",
            "dry_run=yes",
            "dry_run=TRUE",
            "dry_run=true&dry_run=false",
            "dry_run=false&dry_run=false",
            "dry_run=true&dry_run",
            "dry_run&x=1",
        ] {
            assert_eq!(parse_dry_run(bad), Err(()), "{bad}");
        }
    }

    #[test]
    fn key_query_is_read_strictly() {
        assert_eq!(parse_key_query("key=a"), Ok("a".to_owned()));
        assert_eq!(parse_key_query("key=a%2Fb+c"), Ok("a/b c".to_owned()));
        assert_eq!(
            parse_key_query("key=a&key=b"),
            Err("key may be given only once")
        );
        assert_eq!(
            parse_key_query("key=a&other=1"),
            Err("unknown flag; use key")
        );
        assert_eq!(parse_key_query("key"), Err("every flag needs a value"));
        assert_eq!(parse_key_query(""), Err("give the key name as ?key=<name>"));
        for bad in ["key=", "key=%zz", "other=1", "other"] {
            assert!(parse_key_query(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn failures_query_defaults_and_filters_are_strict() {
        assert_eq!(parse_failures_query(""), Ok((64, None)));
        assert_eq!(parse_failures_query("limit=1"), Ok((1, None)));
        assert_eq!(
            parse_failures_query("key=a%2Fb"),
            Ok((64, Some("a/b".to_owned())))
        );
        assert_eq!(
            parse_failures_query("limit=64&key=a"),
            Ok((64, Some("a".to_owned())))
        );
        for bad in [
            "limit=0",
            "limit=65",
            "limit=x",
            "limit",
            "limit=1&limit=2",
            "key=",
            "key=%zz",
            "key=a&key=b",
            "other=1",
            "other",
        ] {
            assert!(parse_failures_query(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_merges_json_fields_without_trimming_the_document() {
        let body = r#"{ "keys":[] }  "#;
        let merged = merge_status_objects(
            body,
            &[
                ("config", r#"{"generation":2}"#),
                ("log", r#"{"write_errors":3}"#),
            ],
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&merged).unwrap(),
            serde_json::json!({
                "keys": [],
                "config": {"generation": 2},
                "log": {"write_errors": 3},
            })
        );
        assert!(
            merge_status_objects(r#"{"keys":[]}"#, &[("log", "not json")]).is_none(),
            "an unmergeable field must fail closed"
        );
        assert!(merge_status_objects("[]", &[]).is_none());
    }

    #[tokio::test]
    async fn an_unknown_admin_path_is_404_for_every_method() {
        use axum::extract::Request;
        use tower_service::Service as _;

        let text = format!(
            "[admin]\nlisten = \"127.0.0.1:4098\"\ntoken_sha256 = \"{}\"\n",
            sha256_hex("admin-token-for-tests")
        );
        let app = App::new(
            crate::config::Config::parse(&text).unwrap(),
            std::collections::HashMap::new(),
            crate::chain::Policy::default(),
            crate::plugins::PluginChain::default(),
        );
        let mut router = router(app);
        for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
            let request = Request::builder()
                .method(method.clone())
                .uri("/v1/no-such-route")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", "admin-token-for-tests"),
                )
                .body(Body::empty())
                .unwrap();
            let response = router.call(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
        }
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
