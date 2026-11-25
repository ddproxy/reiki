use std::fmt::Write as _;
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
    (
        "/stream-console",
        "get",
        "Read-only stream usage, credentials and audit tail for reiki top; either admin token",
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

/// The credential class a request presented. The read-only class opens the read routes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenClass {
    Full,
    ReadOnly,
}

impl TokenClass {
    fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ReadOnly => "read_only",
        }
    }
}

/// What a route does. A mutating route refuses the read-only token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Mutate,
}

/// Header a caller may set to name itself in the audit log. Never used for authorization.
const ACTOR_HEADER: &str = "x-reiki-actor";
const ACTOR_MAX_CHARS: usize = 64;

/// The one error shape of every admin route: `{"ok":false,"error":{"code","message"}}`. `code` is a
/// stable snake_case identifier clients may branch on; `message` is fixed or operator-facing text,
/// never a token, hash or caller-supplied value outside what the request already named.
pub(crate) fn error_envelope(code: &str, message: &str) -> String {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}}).to_string()
}

pub(crate) fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    text_response(status, "application/json", error_envelope(code, message))
}

/// The caller's audit label: `X-Reiki-Actor` cut to 64 characters, with anything outside
/// `[A-Za-z0-9._@:-]` replaced by `_`. Invalid UTF-8 is replaced rather than dropped, so the label
/// still says something. Absent or empty means no label.
pub(crate) fn actor_label(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(ACTOR_HEADER)?;
    let text = String::from_utf8_lossy(raw.as_bytes());
    let label: String = text
        .trim()
        .chars()
        .take(ACTOR_MAX_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':') {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!label.is_empty()).then_some(label)
}

/// One structured record per mutating request: the credential class that acted, the caller's
/// label and the outcome. The label is for people reading the log; it grants nothing.
fn audit_mutation(action: &'static str, class: TokenClass, actor: Option<&str>, outcome: &str) {
    tracing::info!(
        target: "reiki::admin_audit",
        action,
        token_class = class.name(),
        actor = actor.unwrap_or(""),
        outcome,
        "admin mutation"
    );
}

fn handler(path: &str, method: &str) -> MethodRouter<Arc<App>> {
    match (path, method) {
        ("/reload", "post") => post(reload),
        ("/keys/enable", "post") => post(enable_key),
        ("/failures", "get") => get(failures),
        ("/stream-console", "get") => get(stream_console),
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

fn error_content() -> serde_json::Value {
    serde_json::json!({"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}})
}

/// The single credential shape returned by stream creation and token additions.
fn token_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type":"object","additionalProperties":false,
        "required":["token"],
        "properties":{"token":{
            "type":"object","additionalProperties":false,
            "required":["id","value","expires_at"],
            "properties":{
                "id":{"type":"string"},
                "value":{"type":"string","description":"The credential is shown only in this response"},
                "expires_at":{"type":"string","format":"date-time","description":"RFC3339 UTC instant"}
            }
        }}
    })
}

fn route_response_schema(path: &str, method: &str) -> serde_json::Value {
    let generic_obj = serde_json::json!({"type": "object", "additionalProperties": true});
    match (path, method) {
        ("/v1/status", "get") => serde_json::json!({
            "type":"object","additionalProperties":true,
            "required":["keys","streams","config","observed","peers","log"],
            "properties":{
                "keys":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":true}},
                "keys_truncated":{"type":"boolean","description":"True when keys were capped at the documented bound"},
                "streams":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":true}},
                "streams_truncated":{"type":"boolean","description":"True when streams were capped at the documented bound"},
                "config":{"type":"object","additionalProperties":true},
                "observed":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":true}},
                "observed_truncated":{"type":"boolean","description":"True when observed entries were capped at the documented bound"},
                "peers":{"type":"object","additionalProperties":true},
                "log":{"type":"object","additionalProperties":true},
                "stream_admin":{"type":"object","additionalProperties":true},
                "intercept":{"type":"object","additionalProperties":true}
            }
        }),
        ("/v1/reload", "post") => generic_obj,
        ("/v1/keys/enable", "post") => serde_json::json!({
            "type":"object","additionalProperties":false,
            "required":["key"],
            "properties":{"key":{"type":"string"}}
        }),
        ("/v1/failures", "get") => serde_json::json!({
            "type":"object","additionalProperties":false,
            "required":["failures","failures_total","failures_truncated"],
            "properties":{
                "failures":{"type":"array","maxItems":crate::failures::MAX_QUERY_LIMIT,
                    "items":{"type":"object","additionalProperties":true}},
                "failures_total":{"type":"integer","minimum":0},
                "failures_truncated":{"type":"boolean"}
            }
        }),
        ("/v1/stream-console", "get") => serde_json::json!({
            "type":"object","additionalProperties":true,
            "required":["enabled","streams","audit"],
            "properties":{
                "enabled":{"type":"boolean"},
                "streams":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":true}},
                "streams_total":{"type":"integer","minimum":0},
                "streams_truncated":{"type":"boolean"},
                "audit":{"type":"array","maxItems":64,"items":{"type":"object","additionalProperties":true}},
                "audit_total":{"type":"integer","minimum":0},
                "audit_truncated":{"type":"boolean"},
                "audit_tail_limit":{"type":"integer","minimum":0}
            }
        }),
        ("/v1/openapi.json", "get") => {
            serde_json::json!({"type":"object","additionalProperties":true})
        }
        (p, "get") if p.ends_with("/metrics") => serde_json::json!({"type":"string"}),
        ("/v1/cost", "get") => generic_obj,
        ("/v1/usage/streams", "get") => generic_obj,
        ("/v1/streams", "get") => serde_json::json!({
            "type":"object","additionalProperties":true,
            "required":["streams"],
            "properties":{
                "streams":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":true}},
                "streams_total":{"type":"integer","minimum":0},
                "streams_truncated":{"type":"boolean"}
            }
        }),
        ("/v1/streams/create", "post") => token_response_schema(),
        (p, "post") if p.ends_with("/tokens") => token_response_schema(),
        (p, _) if p.starts_with("/v1/streams") => serde_json::json!({
            "type":"object","additionalProperties":false
        }),
        _ => generic_obj,
    }
}

fn operation(summary: &str) -> serde_json::Value {
    serde_json::json!({
        "summary": summary,
        "security": [{"bearerAuth": []}],
        "responses": {
            "200": {"description": "OK"},
            "401": {"description": "Missing or wrong token", "content": error_content()}
        },
    })
}

/// The 403 text of a mutating route, for OpenAPI.
const FULL_TOKEN_ONLY: &str =
    "The read-only token was presented; this route needs the full admin token";

/// The OpenAPI 3.1 description of the v1 routes, including those plugins add.
async fn openapi(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, "/v1/openapi.json", Access::Read, &headers) {
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
            op["responses"]["403"] = serde_json::json!({"description": FULL_TOKEN_ONLY});
            op["responses"]["422"] =
                serde_json::json!({"description": "The config was refused; nothing changed"});
        }
        if *path == "/failures" {
            op["parameters"] = serde_json::json!([
                {
                    "name": "limit", "in": "query", "required": false,
                    "schema": {"type": "integer", "minimum": 1, "maximum": crate::failures::MAX_QUERY_LIMIT},
                    "description": "Records to return, newest first; defaults to 64",
                },
                {
                    "name": "key", "in": "query", "required": false,
                    "schema": {"type": "string"},
                    "description": "Only failures for this key name",
                },
                {
                    "name": "stream", "in": "query", "required": false,
                    "schema": {"type": "string"},
                    "description": "Only failures for this stream name",
                },
            ]);
            op["responses"]["400"] = serde_json::json!({
                "description": "limit is not 1..=64, a flag is repeated, or a name is empty or invalid percent-encoding"
            });
        }
        if *path == "/stream-console" {
            op["responses"]["200"]["description"] = serde_json::json!(
                "Read-only streams, safe credential ids and bounded audit tail; enabled is false without [stream_admin]"
            );
            op["responses"]["500"] =
                serde_json::json!({"description": "The audit chain could not be verified"});
        }
        if *path == "/keys/enable" {
            op["parameters"] = serde_json::json!([{
                "name": "key", "in": "query", "required": true,
                "schema": {"type": "string"},
            }]);
            op["responses"]["400"] = serde_json::json!({
                "description": "Missing, repeated or unknown flag, or a name that is not valid percent-encoding"
            });
            op["responses"]["403"] = serde_json::json!({"description": FULL_TOKEN_ONLY});
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
    for (path, item) in paths.iter_mut() {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        for (method, op) in obj.iter_mut() {
            if !matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                continue;
            }
            let Some(responses) = op.get_mut("responses").and_then(Value::as_object_mut) else {
                continue;
            };
            for (status, response) in responses.iter_mut() {
                if status.starts_with('2') {
                    if response.get("content").is_none() {
                        let schema = route_response_schema(path, method.as_str());
                        let is_metrics = path.ends_with("/metrics") && method == "get";
                        if is_metrics {
                            response["content"] =
                                serde_json::json!({"text/plain": {"schema": schema}});
                        } else if path == "/v1/cost" && method == "get" {
                            response["content"] = serde_json::json!({
                                "application/json": {"schema": schema.clone()},
                                "text/csv": {"schema": {"type":"string"}}
                            });
                        } else {
                            response["content"] =
                                serde_json::json!({"application/json": {"schema": schema}});
                        }
                    }
                } else if (status.starts_with('4') || status.starts_with('5'))
                    && response.get("content").is_none()
                {
                    response["content"] = error_content();
                }
            }
        }
    }
    #[allow(unused_mut)]
    let mut document = serde_json::json!({
        "openapi": "3.1.0",
        "info": {"title": "Reiki admin API", "version": VERSION},
        "components": {
            "securitySchemes": {"bearerAuth": {
                "type": "http", "scheme": "bearer",
                "description": "The full admin token, or the read-only token on read routes. Errors use the admin error envelope.",
            }},
            "schemas": {
                "Error": {
                    "type": "object",
                    "additionalProperties": true,
                    "required": ["code", "message"],
                    "properties": {
                        "code": {"type": "string"},
                        "message": {"type": "string"}
                    }
                },
                "ErrorEnvelope": {
                    "type": "object",
                    "additionalProperties": true,
                    "required": ["ok", "error"],
                    "properties": {
                        "ok": {"type": "boolean", "const": false},
                        "error": {"$ref": "#/components/schemas/Error"}
                    }
                }
            }
        },
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
    let class = match authorize(&app, peer, "/status", Access::Read, &headers) {
        Ok(class) => class,
        Err(refusal) => return *refusal,
    };
    let redact = class == TokenClass::ReadOnly;
    let snapshot = app.snapshot();
    let now = Instant::now();
    let body = {
        let mut health = app
            .health()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        build_status(&snapshot, app.gateway(), &mut health, now)
    };
    let config = app.reload_status_json(redact);
    let intercept = app.ca_not_after().map(|not_after| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let days_left = (not_after - now).div_euclid(86_400);
        serde_json::json!({
            "ca_not_after_at": crate::reload::rfc3339_i64(not_after),
            "ca_days_left": days_left,
        })
        .to_string()
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
                let _ = file;
                serde_json::json!({
                    "ca_not_after_at": crate::reload::rfc3339_i64(not_after),
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
        // A state error names the state file's path and parse position; read-only callers get a code.
        let state_error = manager.state_error().map(|error| {
            if redact {
                "state_unavailable".to_owned()
            } else {
                error
            }
        });
        serde_json::json!({
            "enabled": true,
            "streams": manager.list().len(),
            "state_error": state_error,
        })
        .to_string()
    });
    if let Some(status) = &stream_status {
        fields.push(("stream_admin", status));
    }
    let Some(body) = merge_status_objects(&body, &fields) else {
        // An unmergeable status must not become a corrupt JSON document on the wire.
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "status_unavailable",
            "the status document could not be assembled",
        );
    };
    let body = cap_status_lists(&body);
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
    let class = match authorize(&app, peer, "/reload", Access::Mutate, &headers) {
        Ok(class) => class,
        Err(refusal) => return *refusal,
    };
    let actor = actor_label(&headers);
    let Ok(dry_run) = parse_dry_run(uri.query().unwrap_or("")) else {
        // Fail closed: an unreadable preview flag must not apply a live reload.
        audit_mutation("reload", class, actor.as_deref(), "refused_dry_run_flag");
        return error_response(
            StatusCode::BAD_REQUEST,
            "bad_dry_run",
            "dry_run must be given once, as true, 1, false or 0",
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
    let action = if dry_run { "reload_preview" } else { "reload" };
    match outcome {
        Ok(Ok(report)) => {
            audit_mutation(action, class, actor.as_deref(), "ok");
            text_response(StatusCode::OK, "application/json", report.to_json())
        }
        Ok(Err(error)) => {
            audit_mutation(action, class, actor.as_deref(), "refused");
            text_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "application/json",
                crate::reload::error_json(&error),
            )
        }
        Err(_) => {
            audit_mutation(action, class, actor.as_deref(), "task_failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "reload_task_failed",
                "the reload task failed",
            )
        }
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
    let class = match authorize(&app, peer, "/keys/enable", Access::Mutate, &headers) {
        Ok(class) => class,
        Err(refusal) => return *refusal,
    };
    let actor = actor_label(&headers);
    let key = match parse_key_query(uri.query().unwrap_or("")) {
        Ok(key) => key,
        Err(e) => {
            audit_mutation("keys_enable", class, actor.as_deref(), "refused_bad_query");
            return error_response(StatusCode::BAD_REQUEST, "bad_request", e);
        }
    };
    // Persisting the state writes and syncs a file, which must not stall the runtime.
    let enabled = {
        let (worker, name) = (Arc::clone(&app), key.clone());
        tokio::task::spawn_blocking(move || worker.enable_key(&name)).await
    };
    let Ok(enabled) = enabled else {
        audit_mutation("keys_enable", class, actor.as_deref(), "task_failed");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "enable_task_failed",
            "the enable task failed",
        );
    };
    if enabled {
        tracing::info!(key, "key re-enabled through the admin listener");
        audit_mutation("keys_enable", class, actor.as_deref(), "ok");
        text_response(
            StatusCode::OK,
            "application/json",
            format!("{{\"key\":{}}}", json_string(&key)),
        )
    } else {
        audit_mutation("keys_enable", class, actor.as_deref(), "unknown_key");
        error_response(StatusCode::NOT_FOUND, "unknown_key", "unknown key")
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
    let class = match authorize(&app, peer, "/failures", Access::Read, &headers) {
        Ok(class) => class,
        Err(refusal) => return *refusal,
    };
    let (limit, key, stream) = match parse_failures_query(uri.query().unwrap_or("")) {
        Ok(query) => query,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, "bad_request", e),
    };
    let mut records = app
        .failures()
        .snapshot(limit, key.as_deref(), stream.as_deref());
    if class == TokenClass::ReadOnly {
        // Snippets are upstream body text; the read-only token gets the facts without them.
        for record in &mut records {
            record.note = None;
        }
    }
    let total = app
        .failures()
        .count_matching(key.as_deref(), stream.as_deref());
    match serde_json::to_value(&records) {
        Ok(rows) => {
            let body = serde_json::json!({
                "failures": rows,
                "failures_total": total,
                "failures_truncated": total > records.len(),
            })
            .to_string();
            text_response(StatusCode::OK, "application/json", body)
        }
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failures_unavailable",
            "cannot write the failure ring",
        ),
    }
}

/// A read-only view of every configured stream for the local `reiki top` screen. It accepts either
/// ordinary admin token, never the stream-management token: nothing here can mutate a stream and
/// no credential value, hash, file path or audit-chain hash is included. For the read-only token
/// the audit records' free-text reasons are withheld.
async fn stream_console(
    State(app): State<Arc<App>>,
    peer: Option<Extension<Peer>>,
    headers: HeaderMap,
) -> Response {
    let peer = peer.map(|Extension(peer)| peer.0);
    let class = match authorize(&app, peer, "/stream-console", Access::Read, &headers) {
        Ok(class) => class,
        Err(refusal) => return *refusal,
    };
    let redact = class == TokenClass::ReadOnly;
    let snapshot = app.snapshot();
    let gateway = app.gateway();
    let manager = app.stream_admin();
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());

    let streams = snapshot
        .config
        .streams
        .iter()
        .map(|(name, stream)| {
            let usage = gateway.stream_state(name);
            let limits = console_limits(stream.limits.as_ref());
            let dynamic = stream.dynamic.is_some();
            let (tokens, expiry, state) = if dynamic {
                let tokens: Vec<Value> = stream
                    .dynamic_tokens
                    .iter()
                    .map(|token| {
                        let state = match token.expires_at {
                            Some(expires) if expires <= now => "expired",
                            _ => "active",
                        };
                        serde_json::json!({
                            "id": token.id,
                            "expires_at": token.expires_at.map(crate::reload::rfc3339_secs),
                            "state": state,
                        })
                    })
                    .collect();
                let live: Vec<u64> = stream
                    .dynamic_tokens
                    .iter()
                    .filter_map(|token| token.expires_at)
                    .filter(|expires| *expires > now)
                    .collect();
                let expiry = live.iter().copied().min();
                let state = if tokens.is_empty() {
                    "revoked"
                } else if live.is_empty() {
                    "expired"
                } else {
                    "active"
                };
                (tokens, expiry, state)
            } else {
                (Vec::new(), None, "active")
            };
            serde_json::json!({
                "name": name,
                "dynamic": dynamic,
                "chain": stream.chain,
                "limits": limits,
                "tokens": tokens,
                "expires_at": expiry.map(crate::reload::rfc3339_secs),
                "rpm": {"current": usage.minute.requests, "limit": stream.limits.as_ref().and_then(|l| l.rpm).map(u64::from)},
                "concurrency": {"current": gateway.active_count(name), "limit": stream.limits.as_ref().and_then(|l| l.concurrent).map(u64::from)},
                "last_used_at": gateway.last_used(name).map(crate::reload::rfc3339_secs),
                "state": state,
            })
        })
        .collect::<Vec<_>>();

    let (audit, audit_total) = match manager {
        Some(manager) => match manager.audit_tail_with_total(64) {
            Ok((records, total)) => (records
                .iter()
                .map(|record| {
                    serde_json::json!({
                        "seq": record.seq,
                        "time_at": record.time,
                        "action": record.action,
                        "stream": record.stream,
                        "outcome": record.outcome,
                        "reason": if redact { Value::Null } else { Value::from(record.reason.clone()) },
                        "changes": record.changes.iter().map(|change| {
                            serde_json::json!({
                                "field": change.field,
                                "from": change.from,
                                "to": change.to,
                            })
                        }).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>(), total),
            Err(_) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "audit_unavailable",
                    "the audit chain could not be verified",
                );
            }
        },
        None => (Vec::new(), 0),
    };
    let streams_total = streams.len();
    let streams_truncated = streams_total > STATUS_LIST_MAX;
    let streams = if streams_truncated {
        streams
            .into_iter()
            .take(STATUS_LIST_MAX)
            .collect::<Vec<_>>()
    } else {
        streams
    };
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "enabled": manager.is_some(),
            "streams": streams,
            "streams_total": streams_total,
            "streams_truncated": streams_truncated,
            "audit": audit,
            "audit_total": audit_total,
            "audit_truncated": audit_total > 64,
            "audit_tail_limit": 64,
        }),
    )
}

/// The numeric ceilings of one stream. `period_zone` and store-error mode are deliberately left
/// out: the console shows limits, not configuration knobs.
fn console_limits(limits: Option<&crate::config::StreamLimits>) -> Value {
    serde_json::json!({
        "rpm": limits.and_then(|l| l.rpm).map(u64::from),
        "tpm": limits.and_then(|l| l.tpm),
        "concurrent": limits.and_then(|l| l.concurrent).map(u64::from),
        "daily_requests": limits.and_then(|l| l.daily_requests).map(u64::from),
        "daily_tokens": limits.and_then(|l| l.daily_tokens),
        "monthly_requests": limits.and_then(|l| l.monthly_requests).map(u64::from),
        "monthly_tokens": limits.and_then(|l| l.monthly_tokens),
    })
}

/// Reads `limit`, `key` and `stream` strictly: a repeated or unknown flag, a bare `limit`, an
/// empty name or invalid percent-encoding answers 400 rather than returning a list that was not
/// asked for.
fn parse_failures_query(
    query: &str,
) -> Result<(usize, Option<String>, Option<String>), &'static str> {
    let mut limit = crate::failures::MAX_QUERY_LIMIT;
    let mut key: Option<String> = None;
    let mut stream: Option<String> = None;
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
                    .filter(|limit| (1..=crate::failures::MAX_QUERY_LIMIT).contains(limit))
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
            "stream" => {
                if stream.is_some() {
                    return Err("stream may be given only once");
                }
                stream = Some(
                    decode_query_value(value)
                        .filter(|stream| !stream.is_empty())
                        .ok_or("the stream name is empty or not valid percent-encoding")?,
                );
            }
            _ => return Err("unknown flag; use limit, key and stream"),
        }
    }
    Ok((limit, key, stream))
}

/// Host-owned gauge appended to `/v1/metrics`, so a closed store is visible even without
/// request traffic to increment a counter.
fn gateway_store_metrics(ok: bool) -> String {
    let mut out = String::from(
        "# HELP reiki_gateway_store_ok Per-stream counter store availability.\n\
                                # TYPE reiki_gateway_store_ok gauge\n",
    );
    let _ = writeln!(out, "reiki_gateway_store_ok {}", u8::from(ok));
    out
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
        return not_found();
    }
    let peer = peer.map(|Extension(peer)| peer.0);
    if let Err(refusal) = authorize(&app, peer, uri.path(), Access::Read, &headers) {
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
        return not_found();
    };
    let usage = snapshot
        .config
        .exposes_usage()
        .then(|| crate::pressure::upstream_usage(&snapshot.config, app.pressure(), now));
    let observed = app.observed().cooldowns(SystemTime::now());
    let mut extra = app
        .stream_admin()
        .map_or_else(String::new, |manager| manager.metrics().render());
    extra.push_str(&gateway_store_metrics(app.gateway().store_ok()));
    extra.push_str(&app.failures().metrics());
    extra.push_str(&app.plugins().metrics_text());
    match app.plugins().on_admin_with_extra(
        path,
        uri.query().unwrap_or_default(),
        keys,
        usage,
        observed,
        extra,
    ) {
        Some(reply) => {
            // A plugin that returns the shared error envelope gets the matching status.
            let status = if reply.content_type.starts_with("application/json")
                && reply.body.trim_start().starts_with("{\"ok\":false")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            text_response(status, reply.content_type, reply.body)
        }
        None => not_found(),
    }
}

/// Checks a request's bearer token against the admin token hashes, logging each failure with the
/// peer and path and counting it for the peer's lockout. Returns the class of the token that
/// matched. The read-only class is refused on a mutating route with 403; a missing or wrong token
/// is 401. The presented token is never logged.
fn authorize(
    app: &App,
    peer: Option<IpAddr>,
    path: &str,
    access: Access,
    headers: &HeaderMap,
) -> Result<TokenClass, Box<Response>> {
    // Check and reserve in one lock, so a burst cannot all pass before any attempt is counted.
    let attempt = peer.map(|ip| app.admin_logins().reserve(ip, Instant::now()));
    if let Some(Err(wait)) = attempt {
        tracing::warn!(?peer, path = %path, "too many failed admin logins, refusing");
        return Err(Box::new(admin_throttled(wait)));
    }
    match token_class(app, headers) {
        Ok(class) => {
            if let Some(ip) = peer {
                app.admin_logins().succeed(ip);
            }
            if access == Access::Mutate && class == TokenClass::ReadOnly {
                tracing::warn!(?peer, path = %path, "read-only admin token refused on a mutating route");
                return Err(Box::new(error_response(
                    StatusCode::FORBIDDEN,
                    "read_only_token",
                    "the read-only token cannot change state; this route needs the full admin token",
                )));
            }
            Ok(class)
        }
        Err(reason) => {
            let failures = attempt.and_then(Result::ok).map(|a| a.failures());
            tracing::warn!(?peer, reason, ?failures, path = %path, "failed admin login");
            Err(Box::new(admin_unauthorized()))
        }
    }
}

/// Which admin token, if any, a request presents. Both hashes are compared in constant time.
fn token_class(app: &App, headers: &HeaderMap) -> Result<TokenClass, &'static str> {
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
        return Ok(TokenClass::Full);
    }
    if let Some(read) = admin.read_token_sha256.as_ref()
        && constant_time_eq(&read.to_ascii_lowercase(), &hash)
    {
        return Ok(TokenClass::ReadOnly);
    }
    Err("wrong token")
}

/// The 401 of every admin route. Missing and wrong credentials get the same body.
pub(crate) fn admin_unauthorized() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "missing or wrong admin token",
    )
}

/// The 429 of a throttled admin client, with the seconds to wait.
pub(crate) fn admin_throttled(wait: Duration) -> Response {
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "too_many_attempts",
        "too many failed logins; wait before retrying",
    );
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    if let Ok(value) = HeaderValue::from_str(&secs.max(1).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", "no such admin route")
}

#[cfg(feature = "peers")]
pub(crate) fn unauthorized() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "missing or wrong peer token",
    )
}

/// A refused client waiting out a login lockout, with the remaining seconds.
#[cfg(feature = "peers")]
pub(crate) fn throttled(wait: Duration) -> Response {
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "too_many_attempts",
        "too many failed logins; wait before retrying",
    );
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

const STATUS_LIST_MAX: usize = 256;

/// Caps status list fields to a documented bound and reports truncation.
fn cap_status_lists(body: &str) -> String {
    let mut doc: Value = match serde_json::from_str(body) {
        Ok(doc) => doc,
        Err(_) => return body.to_owned(),
    };
    let Some(obj) = doc.as_object_mut() else {
        return body.to_owned();
    };
    for key in ["keys", "streams", "observed"] {
        let truncated = obj
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|arr| arr.len() > STATUS_LIST_MAX);
        if truncated && let Some(Value::Array(arr)) = obj.get_mut(key) {
            arr.truncate(STATUS_LIST_MAX);
        }
        obj.insert(format!("{key}_truncated"), Value::Bool(truncated));
    }
    doc.to_string()
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
                out.push_str(",\"state\":\"cooling\",\"cooldown_seconds\":");
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
            out.push_str(",\"cooldown_seconds\":");
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
        return Err(Box::new(admin_unauthorized()));
    };
    let now = Instant::now();
    if let Err(wait) = app.stream_rpm().reserve(source, now) {
        manager.note_denied(source, action, stream, "rate_limited");
        return Err(Box::new(admin_throttled(wait)));
    }
    if let Err(wait) = app.stream_logins().reserve(source, now) {
        manager.note_denied(source, action, stream, "locked_out");
        return Err(Box::new(admin_throttled(wait)));
    }
    let expected = manager.config().token_sha256.to_ascii_lowercase();
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token);
    let matched = supplied.is_some_and(|token| constant_time_eq(&expected, &sha256_hex(token)));
    if !matched {
        manager.note_denied(source, action, stream, "credential_refused");
        return Err(Box::new(admin_unauthorized()));
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
            return Err(Box::new(admin_unauthorized()));
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
            .unwrap_or_else(|_| error_envelope("serialization_failed", "cannot write the answer")),
    )
}

fn api_error_response(error: ApiError) -> Response {
    error_response(error.status, error_code(error), error.message)
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
        return admin_unauthorized();
    };
    let streams = manager.list();
    let streams_total = streams.len();
    let streams_truncated = streams_total > STATUS_LIST_MAX;
    let streams = if streams_truncated {
        streams
            .into_iter()
            .take(STATUS_LIST_MAX)
            .collect::<Vec<_>>()
    } else {
        streams
    };
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "streams": streams,
            "streams_total": streams_total,
            "streams_truncated": streams_truncated,
        }),
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
        return admin_unauthorized();
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
        return admin_unauthorized();
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
        return admin_unauthorized();
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
        return admin_unauthorized();
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
        return admin_unauthorized();
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
        return admin_unauthorized();
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
        return admin_unauthorized();
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
    fn gateway_store_gauge_shows_both_states() {
        let ok = gateway_store_metrics(true);
        let closed = gateway_store_metrics(false);
        assert!(ok.contains("# TYPE reiki_gateway_store_ok gauge\n"));
        assert!(ok.ends_with("reiki_gateway_store_ok 1\n"));
        assert!(closed.ends_with("reiki_gateway_store_ok 0\n"));
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
        assert_eq!(parse_failures_query(""), Ok((64, None, None)));
        assert_eq!(parse_failures_query("limit=1"), Ok((1, None, None)));
        assert_eq!(
            parse_failures_query("key=a%2Fb"),
            Ok((64, Some("a/b".to_owned()), None))
        );
        assert_eq!(
            parse_failures_query("stream=s%2Ft"),
            Ok((64, None, Some("s/t".to_owned())))
        );
        assert_eq!(
            parse_failures_query("limit=64&key=a&stream=s"),
            Ok((64, Some("a".to_owned()), Some("s".to_owned())))
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
            "stream=",
            "stream=%zz",
            "stream=s&stream=t",
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

    fn actor_headers(value: &[u8]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACTOR_HEADER,
            HeaderValue::from_bytes(value).expect("a valid header value"),
        );
        headers
    }

    #[test]
    fn the_actor_label_keeps_a_safe_charset_and_drops_the_rest() {
        assert_eq!(
            actor_label(&actor_headers(b"ops.bot_1@host:9-x")).as_deref(),
            Some("ops.bot_1@host:9-x")
        );
        assert_eq!(
            actor_label(&actor_headers(b"a b;rm\"'<>\\")).as_deref(),
            Some("a_b_rm_____")
        );
        // Non-ASCII and invalid UTF-8 never reach the log as-is.
        assert_eq!(
            actor_label(&actor_headers(b"x\xc3\xa9y\xff")).as_deref(),
            Some("x_y_")
        );
    }

    #[test]
    fn the_actor_label_is_capped_and_absent_when_empty() {
        let long = "a".repeat(200);
        let label = actor_label(&actor_headers(long.as_bytes())).unwrap();
        assert_eq!(label.chars().count(), ACTOR_MAX_CHARS);
        assert_eq!(actor_label(&actor_headers(b"   ")), None);
        assert_eq!(actor_label(&HeaderMap::new()), None);
    }

    #[test]
    fn the_error_envelope_has_one_shape() {
        let body: Value =
            serde_json::from_str(&error_envelope("unknown_key", "unknown key")).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "ok": false,
                "error": {"code": "unknown_key", "message": "unknown key"},
            })
        );
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
        assert!(status["peers"]["tls_ca"][0].get("file").is_none());
        assert_eq!(
            status["peers"]["tls_ca"][0]["ca_not_after_at"],
            "2033-05-19T03:33:20Z"
        );
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
