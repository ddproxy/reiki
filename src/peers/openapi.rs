//! The peer routes in the v1 OpenAPI description, present only while publishing.

use serde_json::{json, Value};

use crate::config::Config;

fn upstream_schema() -> Value {
    json!({
        "type": "object",
        "required": ["name", "healthy_keys", "cooling_keys", "disabled_keys",
                     "soonest_cooldown_seconds", "last_status_class"],
        "properties": {
            "name": {"type": "string"},
            "healthy_keys": {"type": "integer", "minimum": 0},
            "cooling_keys": {"type": "integer", "minimum": 0},
            "disabled_keys": {"type": "integer", "minimum": 0},
            "soonest_cooldown_seconds": {"type": ["integer", "null"], "minimum": 1},
            "last_status_class": {"enum": ["2xx", "4xx", "5xx", null]},
            "keys": {
                "description": "Only with expose_key_labels",
                "type": "array",
                "items": {"type": "object", "properties": {
                    "name": {"type": "string"},
                    "state": {"enum": ["available", "cooling", "out_of_rotation"]},
                    "cooldown_seconds": {"type": ["integer", "null"]},
                }},
            },
        },
    })
}

fn health_schema() -> Value {
    json!({
        "type": "object",
        "required": ["instance", "observed_at", "upstreams"],
        "properties": {
            "instance": {"type": "string"},
            "observed_at": {"type": "string", "format": "date-time"},
            "upstreams": {"type": "array", "items": upstream_schema()},
            "host": {
                "description": "Only with expose_host_stats",
                "type": "object",
                "properties": {
                    "load_average": {"type": ["array", "null"], "items": {"type": "number"}},
                    "cores": {"type": "integer"},
                    "memory": {"type": ["object", "null"], "properties": {
                        "total_bytes": {"type": "integer"},
                        "available_bytes": {"type": "integer"},
                        "swap_total_bytes": {"type": "integer"},
                        "swap_free_bytes": {"type": "integer"},
                    }},
                    "disks": {"type": "array", "items": {"type": "object", "properties": {
                        "roles": {"type": "array", "items": {"enum": ["config", "state", "log", "requestlog"]}},
                        "total_bytes": {"type": "integer"},
                        "available_bytes": {"type": "integer"},
                        "used_percent": {"type": "number"},
                    }}},
                },
            },
            "sessions": {
                "description": "Only with expose_host_stats; streams only with expose_key_labels too",
                "type": "object",
                "properties": {
                    "window_seconds": {"type": "integer"},
                    "active_sessions": {"type": "integer"},
                    "streams": {"type": "array", "items": {"type": "object", "properties": {
                        "stream": {"type": "string"},
                        "active_sessions": {"type": "integer"},
                    }}},
                },
            },
        },
    })
}

/// `(path, path item)` for each peer route, each carrying the peer listener as its server.
pub fn openapi_paths(config: &Config) -> Vec<(String, Value)> {
    let Some(listen) = config
        .peers
        .as_ref()
        .filter(|p| p.publishing())
        .and_then(|p| p.listen.as_deref())
    else {
        return Vec::new();
    };
    let servers = json!([{
        "url": format!("http://{listen}"),
        "description": "The peer listener; these routes are not served on the admin listener",
    }]);
    let health = json!({
        "servers": servers,
        "get": {
            "summary": "Coarse key health per upstream, for peers",
            "security": [{"peerAuth": []}],
            "responses": {
                "200": {"description": "OK", "content": {"application/json": {"schema": health_schema()}}},
                "401": {"description": "Missing or wrong peer token"},
                "429": {"description": "Too many failed peer logins from this address"},
            },
        },
    });
    let stream = json!({
        "servers": servers,
        "get": {
            "summary": "Server-Sent Events: a snapshot, then key, sessions and host events",
            "description": "event: snapshot carries the /v1/health document; event: key carries {observed_at, upstream} for one upstream whose key counts or states changed; event: sessions and event: host carry {observed_at, sessions|host} with expose_host_stats; a `: heartbeat` comment comes every 15 s",
            "security": [{"peerAuth": []}],
            "responses": {
                "200": {"description": "OK", "content": {"text/event-stream": {"schema": {"type": "string"}}}},
                "401": {"description": "Missing or wrong peer token"},
                "429": {"description": "Too many failed logins, or max_subscribers_per_peer streams open from this address, or max_subscribers_per_token with this token"},
                "503": {"description": "max_subscribers streams are open"},
            },
        },
    });
    vec![
        (super::publisher::HEALTH_PATH.to_owned(), health),
        (super::stream::STREAM_PATH.to_owned(), stream),
    ]
}
