use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Path, State};
use axum::http::Method;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::Response;
use axum::routing::{MethodRouter, get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::{Peer, bearer_token, constant_time_eq, sha256_hex};
use crate::chain::{KeyState, KeyStatus};
use crate::config::StrategyConfig;
use crate::gateway::Gateway;
use crate::proxy::{App, Snapshot};
use crate::stream_admin::{self, ApiError, Mutation};
use reiki_plugin::KeyCounts;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prefix of every admin route.
const V1: &str = "/v1";

/// The routes this file serves itself, under [`V1`]. The OpenAPI document is built from this
/// table, so the two cannot drift.
const ROUTES: &[(&str, &str, &str)] = &[
    (
        "/status",
        "get",
        "Key, observed upstream, stream limits, usage, config and reload status",
    ),
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
    bare == "/openapi.json"
        || bare == "/streams"
        || bare.starts_with("/streams/")
        || ROUTES.iter().any(|(route, _, _)| bare == *route)
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
        .route(&format!("{V1}/streams"), get(stream_list))
        .route(&format!("{V1}/streams/create"), post(stream_create))
        .route(
            &format!("{V1}/streams/{{name}}"),
            get(stream_show).put(stream_update).delete(stream_delete),
        )
        .route(
            &format!("{V1}/streams/{{name}}/tokens"),
            post(stream_token_add),
        )
        .route(
            &format!("{V1}/streams/{{name}}/tokens/{{token_id}}/revoke"),
            post(stream_token_revoke),
        )
        .route(
            &format!("{V1}/streams/{{name}}/revoke"),
            post(stream_revoke_all),
        )
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
        let mut op = operation("Provided by a plugin");
        if path == "/cost" {
            op["summary"] = serde_json::json!(
                "Estimated spend from the operator price table, not provider invoices"
            );
            op["parameters"] = serde_json::json!([
                {
                    "name": "period", "in": "query", "required": false,
                    "schema": {"type": "string", "pattern": "^[0-9]{4}-[0-9]{2}(-[0-9]{2}(T[0-9]{2})?)?$"},
                    "description": "UTC month (default), day or hour to report",
                },
                {
                    "name": "stream", "in": "query", "required": false,
                    "schema": {"type": "string"},
                    "description": "Only this configured stream",
                },
                {
                    "name": "upstream", "in": "query", "required": false,
                    "schema": {"type": "string"},
                    "description": "Only this upstream",
                },
                {
                    "name": "format", "in": "query", "required": false,
                    "schema": {"type": "string", "enum": ["json", "csv"]},
                    "description": "JSON (default) or CSV export",
                },
            ]);
            op["responses"]["200"]["description"] =
                serde_json::json!("application/json or text/csv; figures are estimates");
            op["responses"]["400"] =
                serde_json::json!({"description": "Invalid or unknown filter"});
        } else if path == "/cost/metrics" {
            op["summary"] =
                serde_json::json!("Prometheus text for estimated spend, with bounded labels");
        }
        paths.insert(format!("{V1}{path}"), serde_json::json!({"get": op}));
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
    let stream_admin_enabled = app.stream_admin().is_some();
    if stream_admin_enabled {
        paths.extend(stream_admin_openapi_paths());
    }
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
    if stream_admin_enabled {
        document["components"]["securitySchemes"]["streamAdminAuth"] = serde_json::json!({
            "type": "http", "scheme": "bearer",
            "description": "Dedicated stream-management token; never the ordinary admin token",
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
        build_status(&snapshot, app.gateway(), &mut health, now)
    };
    let config = app.reload_status_json();
    let intercept = app.ca_not_after().map(|not_after| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let days_left = (not_after - now).div_euclid(86_400);
        format!("{{\"ca_not_after\":{not_after},\"ca_days_left\":{days_left}}}")
    });
    let observed =
        serde_json::to_string(&app.observed().snapshot()).unwrap_or_else(|_| "[]".into());
    let peer_ca = serde_json::to_value(
        app.peer_ca()
            .into_iter()
            .map(|(file, not_after)| {
                let now = SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
                serde_json::json!({
                    "file": file,
                    "ca_not_after": not_after,
                    "ca_days_left": (not_after - now).div_euclid(86_400),
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
    .to_string();
    let log = format!(r#"{{"write_errors":{}}}"#, app.file_write_errors());
    let plugin_fields: Vec<(String, String)> = app
        .plugins()
        .status_fragments()
        .into_iter()
        .flat_map(|fragment| match fragment.as_object() {
            Some(object) => object
                .iter()
                .map(|(name, value)| (name.clone(), value.to_string()))
                .collect(),
            None => Vec::new(),
        })
        .collect();
    let mut fields: Vec<(&str, &str)> = Vec::new();
    for (name, value) in &plugin_fields {
        fields.push((name.as_str(), value.as_str()));
    }
    fields.push(("config", &config));
    fields.push(("observed", &observed));
    let peers_status = format!(r#"{{"tls_ca":{peer_ca}}}"#);
    fields.push(("peers", &peers_status));
    if let Some(intercept) = &intercept {
        fields.push(("intercept", intercept));
    }
    fields.push(("log", &log));
    let stream_status = app.stream_admin().map(|manager| {
        serde_json::json!({
            "enabled": true,
            "streams": manager.list().len(),
            "state_error": manager.state_error(),
        })
        .to_string()
    });
    if let Some(status) = &stream_status {
        fields.push(("stream_admin", status));
    }
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
    let observed = app.observed().cooldowns(SystemTime::now());
    let mut extra = app
        .stream_admin()
        .map_or_else(String::new, |manager| manager.metrics().render());
    extra.push_str(&app.plugins().metrics_text());
    match app.plugins().on_admin_with_extra(
        path,
        uri.query().unwrap_or_default(),
        keys,
        usage,
        observed,
        extra,
    ) {
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

/// OpenAPI descriptions for the opt-in stream-management API. Included only when the feature is
/// configured, so an unconfigured instance documents exactly the routes it serves.
fn stream_admin_openapi_paths() -> serde_json::Map<String, Value> {
    let mut paths = serde_json::Map::new();
    let read = |summary: &str| {
        serde_json::json!({
            "summary": summary,
            "security": [{"streamAdminAuth": []}],
            "responses": {"200": {"description": "OK"}, "401": {"description": "Missing or wrong credential"}},
        })
    };
    let write = |summary: &str| {
        let mut op = read(summary);
        op["parameters"] = serde_json::json!([
            {
                "name": "Idempotency-Key", "in": "header", "required": true,
                "schema": {"type": "string", "minLength": 1, "maxLength": 128},
            },
            {
                "name": "X-Confirm-Token", "in": "header", "required": false,
                "description": "Second token, required when two-party confirmation is configured",
            },
        ]);
        op["responses"]["400"] =
            serde_json::json!({"description": "Invalid body, reason or idempotency key"});
        op["responses"]["409"] = serde_json::json!({"description": "Duplicate idempotency key with a different request"});
        op["responses"]["422"] =
            serde_json::json!({"description": "A configured ceiling was exceeded"});
        op["responses"]["503"] =
            serde_json::json!({"description": "Audit log or state file write failed"});
        op
    };
    paths.insert(
        format!("{V1}/streams"),
        serde_json::json!({"get": read("List dynamic streams")}),
    );
    paths.insert(
        format!("{V1}/streams/create"),
        serde_json::json!({"post": write("Create a dynamic stream and return its first token once")}),
    );
    paths.insert(
        format!("{V1}/streams/{{name}}"),
        serde_json::json!({
            "get": read("Show one dynamic stream"),
            "put": write("Update a dynamic stream's key or limits"),
            "delete": write("Delete a dynamic stream"),
        }),
    );
    paths.insert(
        format!("{V1}/streams/{{name}}/tokens"),
        serde_json::json!({"post": write("Add a token to a dynamic stream")}),
    );
    paths.insert(
        format!("{V1}/streams/{{name}}/tokens/{{token_id}}/revoke"),
        serde_json::json!({"post": write("Revoke one token")}),
    );
    paths.insert(
        format!("{V1}/streams/{{name}}/revoke"),
        serde_json::json!({"post": write("Revoke every token on a dynamic stream")}),
    );
    paths
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
        // Plugin fragments are objects; the built-in `observed` field is an array.
        if !value.is_object() && !value.is_array() {
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
fn build_status(
    snapshot: &Snapshot,
    gateway: &Gateway,
    health: &mut KeyState,
    now: Instant,
) -> String {
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
        out.push_str(",\"limits\":{");
        let limits = stream.limits.as_ref();
        let numeric = [
            ("rpm", limits.and_then(|l| l.rpm).map(u64::from)),
            ("tpm", limits.and_then(|l| l.tpm)),
            (
                "concurrent",
                limits.and_then(|l| l.concurrent).map(u64::from),
            ),
            (
                "daily_requests",
                limits.and_then(|l| l.daily_requests).map(u64::from),
            ),
            ("daily_tokens", limits.and_then(|l| l.daily_tokens)),
            (
                "monthly_requests",
                limits.and_then(|l| l.monthly_requests).map(u64::from),
            ),
            ("monthly_tokens", limits.and_then(|l| l.monthly_tokens)),
        ];
        for (index, (field, value)) in numeric.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&json_string(field));
            out.push(':');
            match value {
                Some(value) => out.push_str(&value.to_string()),
                None => out.push_str("null"),
            }
        }
        if let Some(limits) = limits
            && let Some(zone) = &limits.period_zone
        {
            out.push_str(",\"period_zone\":");
            out.push_str(&json_string(zone));
        }
        out.push_str("},\"usage\":{");
        let usage = gateway.stream_state(name);
        out.push_str("\"requests\":");
        out.push_str(&usage.requests_total.to_string());
        out.push_str(",\"tokens\":");
        out.push_str(&usage.tokens_total.to_string());
        out.push_str(",\"minute_requests\":");
        out.push_str(&usage.minute.requests.to_string());
        out.push_str(",\"minute_tokens\":");
        out.push_str(&usage.minute.tokens.to_string());
        out.push('}');
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

// ---------------------------------------------------------------------------
// Opt-in stream management. These routes are mounted on the admin listener only and use a
// credential separate from the ordinary admin token.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestLimits {
    #[serde(default)]
    rpm: Option<u32>,
    #[serde(default)]
    tpm: Option<u64>,
    #[serde(default)]
    concurrent: Option<u32>,
    #[serde(default)]
    daily_requests: Option<u32>,
    #[serde(default)]
    daily_tokens: Option<u64>,
    #[serde(default)]
    monthly_requests: Option<u32>,
    #[serde(default)]
    monthly_tokens: Option<u64>,
}

impl RequestLimits {
    fn into_stream_limits(self) -> crate::config::StreamLimits {
        crate::config::StreamLimits {
            rpm: self.rpm,
            tpm: self.tpm,
            concurrent: self.concurrent,
            daily_requests: self.daily_requests,
            daily_tokens: self.daily_tokens,
            monthly_requests: self.monthly_requests,
            monthly_tokens: self.monthly_tokens,
            period_zone: None,
            on_store_error: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateStreamRequest {
    name: String,
    key: String,
    reason: String,
    #[serde(default)]
    ttl: Option<u64>,
    #[serde(default)]
    limits: Option<RequestLimits>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateStreamRequest {
    reason: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    limits: Option<RequestLimits>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRequest {
    reason: String,
    #[serde(default)]
    ttl: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonRequest {
    reason: String,
}

/// Fuzz-only entry point for the strict stream request DTOs. Kept beside the private types so
/// they do not have to become public; the `fuzzing` module forwards to it.
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_decode_stream_requests(data: &[u8]) {
    // Arbitrary bytes are refused through the same strict DTOs the admin routes use.
    let _ = serde_json::from_slice::<CreateStreamRequest>(data);
    let _ = serde_json::from_slice::<UpdateStreamRequest>(data);
    let _ = serde_json::from_slice::<TokenRequest>(data);
    let _ = serde_json::from_slice::<ReasonRequest>(data);

    // Independently of whether the fuzz input decoded, a well-formed request with one extra
    // property must still be refused. The prefix cannot collide with any real DTO field.
    let text: String = String::from_utf8_lossy(data).chars().take(256).collect();
    let unknown = format!("__reiki_fuzz_unknown__{text}__");
    macro_rules! assert_strict {
        ($dto:ty, $payload:expr, $label:expr) => {{
            let payload = $payload;
            let valid = serde_json::to_vec(&payload).expect("fuzz payload serializes");
            assert!(
                serde_json::from_slice::<$dto>(&valid).is_ok(),
                concat!($label, " baseline must decode")
            );
            let mut with_unknown = payload;
            with_unknown
                .as_object_mut()
                .expect("fuzz payload is an object")
                .insert(unknown.clone(), Value::Null);
            let mutated = serde_json::to_vec(&with_unknown).expect("fuzz payload serializes");
            assert!(
                serde_json::from_slice::<$dto>(&mutated).is_err(),
                concat!($label, " accepted an unknown field")
            );
        }};
    }
    assert_strict!(
        CreateStreamRequest,
        serde_json::json!({"name": text, "key": text, "reason": text}),
        "CreateStreamRequest"
    );
    assert_strict!(
        UpdateStreamRequest,
        serde_json::json!({"reason": text}),
        "UpdateStreamRequest"
    );
    assert_strict!(
        TokenRequest,
        serde_json::json!({"reason": text, "ttl": 1_u64}),
        "TokenRequest"
    );
    assert_strict!(
        ReasonRequest,
        serde_json::json!({"reason": text}),
        "ReasonRequest"
    );
}

fn stream_source(peer: Option<IpAddr>) -> IpAddr {
    peer.unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

/// Authenticates one stream-management request with its own token and throttle. Missing and
/// wrong credentials are deliberately indistinguishable on the wire.
fn authorize_stream(
    app: &App,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    action: &'static str,
    stream: &str,
    mutating: bool,
) -> Result<(), Box<Response>> {
    let source = stream_source(peer);
    let Some(manager) = app.stream_admin() else {
        return Err(Box::new(unauthorized()));
    };
    let now = Instant::now();
    if let Err(wait) = app.stream_rpm().reserve(source, now) {
        manager.note_denied(source, action, stream, "rate_limited");
        return Err(Box::new(throttled(wait)));
    }
    if let Err(wait) = app.stream_logins().reserve(source, now) {
        manager.note_denied(source, action, stream, "locked_out");
        return Err(Box::new(throttled(wait)));
    }
    let expected = manager.config().token_sha256.to_ascii_lowercase();
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token);
    let matched = supplied.is_some_and(|token| constant_time_eq(&expected, &sha256_hex(token)));
    if !matched {
        manager.note_denied(source, action, stream, "credential_refused");
        return Err(Box::new(unauthorized()));
    }
    if mutating && !manager.config().require_two_party.is_empty() {
        let supplied = headers
            .get("x-confirm-token")
            .and_then(|value| value.to_str().ok())
            .map(sha256_hex);
        let mut confirmed = false;
        if let Some(hash) = supplied {
            for candidate in &manager.config().require_two_party {
                if constant_time_eq(&candidate.to_ascii_lowercase(), &hash) {
                    confirmed = true;
                }
            }
        }
        if !confirmed {
            manager.note_denied(source, action, stream, "confirmation_refused");
            return Err(Box::new(unauthorized()));
        }
    }
    app.stream_rpm().succeed(source);
    app.stream_logins().succeed(source);
    Ok(())
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    text_response(
        status,
        "application/json",
        serde_json::to_string(&body)
            .unwrap_or_else(|_| r#"{"error":"serialization_failed"}"#.into()),
    )
}

fn api_error_response(error: ApiError) -> Response {
    json_response(
        error.status,
        serde_json::json!({"error": {"type": "stream_admin_refused", "message": error.message}}),
    )
}

fn error_code(error: ApiError) -> &'static str {
    match error.status {
        StatusCode::BAD_REQUEST => "body_refused",
        StatusCode::NOT_FOUND => "unknown_target",
        StatusCode::CONFLICT => "conflict",
        StatusCode::UNPROCESSABLE_ENTITY => "ceiling_refused",
        StatusCode::SERVICE_UNAVAILABLE => "store_failed",
        _ => "refused",
    }
}

fn finish_mutation(
    manager: &stream_admin::Manager,
    source: IpAddr,
    action: &'static str,
    stream: &str,
    result: Result<serde_json::Value, ApiError>,
) -> Response {
    match result {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => {
            manager.note_failed(source, action, stream, error_code(error));
            api_error_response(error)
        }
    }
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, Box<Response>> {
    let Some(value) = headers.get("idempotency-key") else {
        return Err(Box::new(api_error_response(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Idempotency-Key is required",
        })));
    };
    let Ok(key) = value.to_str() else {
        return Err(Box::new(api_error_response(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Idempotency-Key is invalid",
        })));
    };
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(Box::new(api_error_response(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "Idempotency-Key is invalid",
        })));
    }
    Ok(key.to_owned())
}

fn valid_reason(reason: &str) -> bool {
    !reason.is_empty() && reason.len() <= 500 && !reason.chars().any(char::is_control)
}

async fn read_json<T: DeserializeOwned>(
    manager: &stream_admin::Manager,
    source: IpAddr,
    action: &'static str,
    stream: &str,
    headers: &HeaderMap,
    body: Body,
    max: usize,
) -> Result<T, Box<Response>> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
    {
        manager.note_failed(source, action, stream, "content_type_refused");
        return Err(Box::new(api_error_response(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: "application/json is required",
        })));
    }
    if let Some(length) = headers.get(header::CONTENT_LENGTH)
        && length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|value| value > max)
    {
        manager.note_failed(source, action, stream, "body_too_large");
        return Err(Box::new(api_error_response(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "request body too large",
        })));
    }
    let bytes = to_bytes(body, max.saturating_add(1)).await.map_err(|_| {
        Box::new(api_error_response(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "request body could not be read",
        }))
    })?;
    if bytes.len() > max {
        manager.note_failed(source, action, stream, "body_too_large");
        return Err(Box::new(api_error_response(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "request body too large",
        })));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        manager.note_failed(source, action, stream, "body_refused");
        Box::new(api_error_response(ApiError::BAD_REQUEST))
    })
}

async fn stream_list(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) = authorize_stream(&app, Some(source), &headers, "list", "", false) {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    json_response(
        StatusCode::OK,
        serde_json::json!({"streams": manager.list()}),
    )
}

async fn stream_show(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) = authorize_stream(&app, Some(source), &headers, "show", &name, false) {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    match manager.show(&name) {
        Some(stream) => json_response(StatusCode::OK, stream),
        None => api_error_response(ApiError::NOT_FOUND),
    }
}

async fn stream_create(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) = authorize_stream(&app, Some(source), &headers, "create", "", true) {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    let max = manager.config().max_body_bytes;
    let request: CreateStreamRequest =
        match read_json(manager, source, "create", "", &headers, body, max).await {
            Ok(request) => request,
            Err(refusal) => return *refusal,
        };
    if !valid_reason(&request.reason) {
        manager.note_failed(source, "create", "", "reason_refused");
        return api_error_response(ApiError::BAD_REQUEST);
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(refusal) => return *refusal,
    };
    let mutation = Mutation::Create {
        name: request.name,
        key: request.key,
        ttl: request.ttl,
        limits: request.limits.map(RequestLimits::into_stream_limits),
    };
    finish_mutation(
        manager,
        source,
        "create",
        mutation.stream(),
        manager.mutate(source, &key, &request.reason, &mutation),
    )
}

async fn stream_update(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) = authorize_stream(&app, Some(source), &headers, "update", &name, true) {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    let max = manager.config().max_body_bytes;
    let request: UpdateStreamRequest =
        match read_json(manager, source, "update", &name, &headers, body, max).await {
            Ok(request) => request,
            Err(refusal) => return *refusal,
        };
    if !valid_reason(&request.reason) {
        manager.note_failed(source, "update", &name, "reason_refused");
        return api_error_response(ApiError::BAD_REQUEST);
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(refusal) => return *refusal,
    };
    let mutation = Mutation::Update {
        name,
        key: request.key,
        limits: request.limits.map(RequestLimits::into_stream_limits),
    };
    finish_mutation(
        manager,
        source,
        "update",
        mutation.stream(),
        manager.mutate(source, &key, &request.reason, &mutation),
    )
}

async fn stream_token_add(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) = authorize_stream(&app, Some(source), &headers, "token.add", &name, true) {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    let max = manager.config().max_body_bytes;
    let request: TokenRequest =
        match read_json(manager, source, "token.add", &name, &headers, body, max).await {
            Ok(request) => request,
            Err(refusal) => return *refusal,
        };
    if !valid_reason(&request.reason) {
        manager.note_failed(source, "token.add", &name, "reason_refused");
        return api_error_response(ApiError::BAD_REQUEST);
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(refusal) => return *refusal,
    };
    let mutation = Mutation::AddToken {
        name,
        ttl: request.ttl,
    };
    finish_mutation(
        manager,
        source,
        "token.add",
        mutation.stream(),
        manager.mutate(source, &key, &request.reason, &mutation),
    )
}

async fn stream_token_revoke(
    State(app): State<Arc<App>>,
    Path((name, token_id)): Path<(String, String)>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) =
        authorize_stream(&app, Some(source), &headers, "token.revoke", &name, true)
    {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    let max = manager.config().max_body_bytes;
    let request: ReasonRequest =
        match read_json(manager, source, "token.revoke", &name, &headers, body, max).await {
            Ok(request) => request,
            Err(refusal) => return *refusal,
        };
    if !valid_reason(&request.reason) {
        manager.note_failed(source, "token.revoke", &name, "reason_refused");
        return api_error_response(ApiError::BAD_REQUEST);
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(refusal) => return *refusal,
    };
    let mutation = Mutation::RevokeToken { name, token_id };
    finish_mutation(
        manager,
        source,
        "token.revoke",
        mutation.stream(),
        manager.mutate(source, &key, &request.reason, &mutation),
    )
}

async fn stream_revoke_all(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) = authorize_stream(&app, Some(source), &headers, "revoke", &name, true) {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    let max = manager.config().max_body_bytes;
    let request: ReasonRequest =
        match read_json(manager, source, "revoke", &name, &headers, body, max).await {
            Ok(request) => request,
            Err(refusal) => return *refusal,
        };
    if !valid_reason(&request.reason) {
        manager.note_failed(source, "revoke", &name, "reason_refused");
        return api_error_response(ApiError::BAD_REQUEST);
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(refusal) => return *refusal,
    };
    let mutation = Mutation::RevokeAll { name };
    finish_mutation(
        manager,
        source,
        "revoke",
        mutation.stream(),
        manager.mutate(source, &key, &request.reason, &mutation),
    )
}

async fn stream_delete(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let source = stream_source(peer.map(|Extension(peer)| peer.0));
    if let Err(refusal) = authorize_stream(&app, Some(source), &headers, "delete", &name, true) {
        return *refusal;
    }
    let Some(manager) = app.stream_admin() else {
        return unauthorized();
    };
    let max = manager.config().max_body_bytes;
    let request: ReasonRequest =
        match read_json(manager, source, "delete", &name, &headers, body, max).await {
            Ok(request) => request,
            Err(refusal) => return *refusal,
        };
    if !valid_reason(&request.reason) {
        manager.note_failed(source, "delete", &name, "reason_refused");
        return api_error_response(ApiError::BAD_REQUEST);
    }
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(refusal) => return *refusal,
    };
    let mutation = Mutation::Delete { name };
    finish_mutation(
        manager,
        source,
        "delete",
        mutation.stream(),
        manager.mutate(source, &key, &request.reason, &mutation),
    )
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
        let with_array = merge_status_objects(r#"{"keys":[]}"#, &[("observed", "[]")]).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&with_array).unwrap(),
            serde_json::json!({"keys": [], "observed": []})
        );
        assert!(
            merge_status_objects(r#"{"keys":[]}"#, &[("log", "not json")]).is_none(),
            "an unmergeable field must fail closed"
        );
        assert!(
            merge_status_objects(r#"{"keys":[]}"#, &[("log", "3")]).is_none(),
            "a scalar field must fail closed"
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

    #[tokio::test]
    async fn status_publishes_observed_upstreams_and_peer_ca_expiry() {
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
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        app.observed().record(
            "zen",
            "acct-a",
            crate::observed::OK,
            None,
            Default::default(),
            now,
        );
        app.set_peer_ca(vec![("/etc/reiki/peer/ca.pem".to_owned(), 2_000_086_400)]);

        let mut router = router(app);
        let response = router
            .call(
                Request::builder()
                    .uri("/v1/status")
                    .header(header::AUTHORIZATION, "Bearer admin-token-for-tests")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status_code = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status_code, StatusCode::OK);
        let status: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status["observed"][0]["upstream"], "zen");
        assert_eq!(status["observed"][0]["account"], "acct-a");
        assert_eq!(status["observed"][0]["mode"], "observed");
        assert_eq!(status["observed"][0]["status_class"], "ok");
        assert_eq!(
            status["peers"]["tls_ca"][0]["file"],
            "/etc/reiki/peer/ca.pem"
        );
        assert_eq!(status["peers"]["tls_ca"][0]["ca_not_after"], 2_000_086_400);
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
