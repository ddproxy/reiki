//! The peer routes in the v1 OpenAPI description, present only while publishing.

use serde_json::{Value, json};

use crate::config::Config;

fn upstream_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["name", "healthy_keys", "cooling_keys", "disabled_keys",
                     "cooling_combinations",
                     "soonest_cooldown_seconds", "last_status_class", "keys_truncated"],
        "properties": {
            "name": {"type": "string"},
            "healthy_keys": {"type": "integer", "minimum": 0},
            "cooling_keys": {"type": "integer", "minimum": 0},
            "disabled_keys": {"type": "integer", "minimum": 0},
            "cooling_combinations": {"type": "integer", "minimum": 0,
                "description": "How many (route, model) combinations are cooling on this upstream's keys; a count only, never names"},
            "soonest_cooldown_seconds": {"type": ["integer", "null"], "minimum": 1,
                "description": "Rounded up to a multiple of cooldown_bucket_secs, unless it is 0"},
            "last_status_class": {"type": ["string", "null"],
                "description": "2xx, 4xx, 5xx, or null before one; readers ignore unknown values"},
            "capacity_group": {"type": "string", "maxLength": 64,
                "pattern": "^[ -~]+$",
                "description": "Opaque, operator-set group for this upstream; only when [upstream.NAME] capacity_group is configured, never derived from a key label or response"},
            "requests_1h": {"type": "integer", "minimum": 0,
                "description": "Attempts in the last 60 minutes; only with expose_usage, a count only"},
            "limited_1h": {"type": "integer", "minimum": 0,
                "description": "Attempts that were rate-limited or began a cooldown in the last 60 minutes; only with expose_usage"},
            "request_buckets": {"type": "array", "items": {"type": "integer", "minimum": 0},
                "minItems": 12, "maxItems": 12,
                "description": "Twelve five-minute attempt counts, oldest first, newest last; only with expose_usage"},
            "tokens_1h": {"type": "integer", "minimum": 0,
                "description": "Input plus output tokens of the responses served by this upstream in the last 60 minutes; only with expose_usage"},
            "token_buckets": {"type": "array", "items": {"type": "integer", "minimum": 0},
                "minItems": 12, "maxItems": 12,
                "description": "Twelve five-minute token counts, oldest first, newest last; only with expose_usage"},
            "request_minutes": {"type": "array", "items": {"type": "integer", "minimum": 0},
                "minItems": 10, "maxItems": 10,
                "description": "Ten one-minute attempt counts ending at the current minute, oldest first, newest last; only with expose_usage"},
            "token_minutes": {"type": "array", "items": {"type": "integer", "minimum": 0},
                "minItems": 10, "maxItems": 10,
                "description": "Ten one-minute token counts ending at the current minute, oldest first, newest last; only with expose_usage"},
            "keys_truncated": {"type": "boolean",
                "description": "True when the labelled keys were capped at 256; present even without expose_key_labels"},
            "keys": {
                "description": "Only with expose_key_labels; capped at 256 entries",
                "type": "array", "maxItems": 256,
                "items": {"type": "object", "additionalProperties": true, "properties": {
                    "name": {"type": "string"},
                    "state": {"type": "string",
                        "description": "available, cooling, out_of_rotation; readers ignore unknown values"},
                    "cooldown_seconds": {"type": ["integer", "null"]},
                    "cooldown_class": {"type": "string",
                        "description": "Only while cooling, and only when the class is known; readers ignore unknown values"},
                    "quota_reset_at": {"type": "string", "format": "date-time",
                        "description": "RFC3339 UTC reset behind a quota cooldown, when the key names a reset schedule or the response supplied Retry-After; only for a cooling quota key"},
                }},
            },
        },
    })
}

fn observed_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["upstream", "account", "mode", "status_class", "observed_at",
                     "cooldown_seconds", "cooldown_class", "quota"],
        "properties": {
            "upstream": {"type": "string"},
            "account": {"type": "string"},
            "mode": {"type": "string"},
            "status_class": {"type": "string"},
            "observed_at": {"type": "string", "format": "date-time",
                "description": "RFC3339 UTC instant"},
            "cooldown_seconds": {"type": ["integer", "null"], "minimum": 0},
            "cooldown_class": {"type": ["string", "null"]},
            "quota": {
                "type": "object",
                "additionalProperties": true,
                "required": ["requests_limit", "requests_remaining", "requests_reset_seconds",
                             "tokens_limit", "tokens_remaining", "tokens_reset_seconds"],
                "properties": {
                    "requests_limit": {"type": ["integer", "null"], "minimum": 0},
                    "requests_remaining": {"type": ["integer", "null"], "minimum": 0},
                    "requests_reset_seconds": {"type": ["integer", "null"], "minimum": 0},
                    "tokens_limit": {"type": ["integer", "null"], "minimum": 0},
                    "tokens_remaining": {"type": ["integer", "null"], "minimum": 0},
                    "tokens_reset_seconds": {"type": ["integer", "null"], "minimum": 0}
                }
            }
        }
    })
}

/// Optional host extension: four fixed filesystem roles at most.
fn host_schema() -> Value {
    json!({
        "description": "Optional non-core extension: present only with expose_host_stats; off by default",
        "x-reiki-extension": "host_stats",
        "x-reiki-default": false,
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "load_average": {"type": ["array", "null"], "maxItems": 3,
                "items": {"type": "number"}},
            "cores": {"type": "integer"},
            "memory": {"type": ["object", "null"], "additionalProperties": true, "properties": {
                "total_bytes": {"type": "integer"},
                "available_bytes": {"type": "integer"},
                "swap_total_bytes": {"type": "integer"},
                "swap_free_bytes": {"type": "integer"},
            }},
            "disks": {"type": "array", "maxItems": 4,
                "items": {"type": "object", "additionalProperties": true,
                "description": "Figures are null for a role whose filesystem could not be queried in time",
                "properties": {
                    "roles": {"type": "array", "maxItems": 4, "items": {"type": "string"},
                        "description": "Up to four fixed roles: config, state, log, requestlog"},
                    "total_bytes": {"type": ["integer", "null"]},
                    "available_bytes": {"type": ["integer", "null"]},
                    "used_percent": {"type": ["number", "null"]},
                }}},
            "disks_truncated": {"type": "boolean",
                "description": "True when the disk list was capped at 4"},
        },
    })
}

/// Optional session extension: stream counts are capped at the documented bound.
fn sessions_schema() -> Value {
    json!({
        "description": "Optional non-core extension: present only with expose_host_stats; streams only with expose_key_labels too. Counts are client-reported and can be inflated by a token holder. Off by default",
        "x-reiki-extension": "host_stats",
        "x-reiki-default": false,
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "window_seconds": {"type": "integer"},
            "active_sessions": {
                "type": "integer",
                "description": "Client-reported distinct values of session_header in the window, not a count of verified devices",
            },
            "streams": {"type": "array", "maxItems": 256,
                "items": {"type": "object", "additionalProperties": true, "properties": {
                    "stream": {"type": "string"},
                    "active_sessions": {
                        "type": "integer",
                        "description": "Client-reported distinct values of session_header for this stream",
                    },
                }}},
            "streams_truncated": {"type": "boolean",
                "description": "True when the session streams were capped at 256"},
        },
    })
}

pub(crate) fn health_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["instance", "reiki_version", "observed_at", "age_seconds", "failures",
                     "upstreams", "upstreams_truncated", "observed", "observed_truncated"],
        "properties": {
            "instance": {"type": "string"},
            "reiki_version": {"type": "string", "maxLength": 32,
                "description": "The publisher's version, sanitised for terminal display"},
            "observed_at": {"type": "string", "format": "date-time",
                "description": "RFC3339 UTC instant"},
            "age_seconds": {"type": "integer", "minimum": 0,
                "description": "How old the figures were when sent; readers judge freshness by it plus their own time since receipt, not by observed_at"},
            "upstreams": {"type": "array", "maxItems": 64, "items": upstream_schema(),
                "description": "Keyed upstreams in config order; capped at 64"},
            "upstreams_truncated": {"type": "boolean",
                "description": "True when keyed upstreams were capped at 64"},
            "observed": {"type": "array", "maxItems": 256, "items": observed_schema(),
                "description": "Passthrough upstreams observed by the publisher; always present, may be empty; capped at 256"},
            "observed_truncated": {"type": "boolean",
                "description": "True when observed entries were capped at 256"},
            "requestlog_export_dropped_total": {
                "type": "integer",
                "minimum": 0,
                "description": "Summary records dropped by the bounded queue or failed syslog sends; present when the request-log export reports drops",
            },
            "failures": {
                "type": "object",
                "description": "Counts of recent key failures by class over a bounded in-memory ring, newest or not; never bodies, key names or streams",
                "additionalProperties": {"type": "integer", "minimum": 0},
            },
            "gateway": {
                "description": "Only with expose_usage; aggregate gateway counters, counts only",
                "type": "object",
                "additionalProperties": true,
                "required": ["requests", "tokens", "store_errors"],
                "properties": {
                    "requests": {"type": "integer", "minimum": 0},
                    "tokens": {"type": "integer", "minimum": 0},
                    "store_errors": {"type": "integer", "minimum": 0}
                }
            },
            "streams": {
                "description": "Only with expose_usage; aggregate stream counts",
                "type": "object",
                "additionalProperties": true,
                "required": ["static_streams", "dynamic_streams"],
                "properties": {
                    "static_streams": {"type": "integer", "minimum": 0},
                    "dynamic_streams": {"type": "integer", "minimum": 0}
                }
            },
            "host": host_schema(),
            "sessions": sessions_schema(),
        },
    })
}

/// The provider-health contract as a standalone OpenAPI document.
///
/// This is the machine-readable contract for `/v1/health` on the peer listener.
/// It is generated from the same schema code as the admin OpenAPI and checked by
/// tests against `docs/provider-health.openapi.json`.
pub fn provider_health_openapi() -> Value {
    let health = json!({
        "get": {
            "summary": "Coarse key health per upstream, for peers",
            "security": [{"peerAuth": []}],
            "responses": {
                "200": {"description": "OK", "content": {"application/json": {"schema": health_schema()}}},
                "401": {"description": "Missing or wrong peer token", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
                "429": {"description": "Too many failed peer logins from this address", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
            },
        },
    });
    let stream = json!({
        "get": {
            "summary": "Server-Sent Events: a snapshot, then key, sessions and host events",
            "description": "event: snapshot carries the /v1/health document; event: key carries {observed_at, age_seconds, upstream} for one upstream whose key counts or states changed; event: sessions and event: host carry {observed_at, sessions|host} with expose_host_stats; a `: heartbeat` comment comes every 15 s",
            "security": [{"peerAuth": []}],
            "responses": {
                "200": {"description": "OK", "content": {"text/event-stream": {"schema": {"type": "string"}}}},
                "401": {"description": "Missing or wrong peer token", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
                "429": {"description": "Too many failed logins, or max_subscribers_per_peer streams open from this address, or max_subscribers_per_token with this token", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
                "503": {"description": "max_subscribers streams are open", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
            },
        },
    });
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Reiki provider health contract",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Contract for the read-only peer listener health document. Readers must ignore unknown fields and unknown enum-like values. Optional fields may be absent; host and sessions are non-core extensions and are off by default."
        },
        "paths": {
            super::publisher::HEALTH_PATH: health,
            super::stream::STREAM_PATH: stream,
        },
        "components": {
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
            },
            "securitySchemes": {
                "peerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "The read-only peer token; accepted on the peer listener only"
                }
            }
        }
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
                "401": {"description": "Missing or wrong peer token", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
                "429": {"description": "Too many failed peer logins from this address", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
            },
        },
    });
    let stream = json!({
        "servers": servers,
        "get": {
            "summary": "Server-Sent Events: a snapshot, then key, sessions and host events",
            "description": "event: snapshot carries the /v1/health document; event: key carries {observed_at, age_seconds, upstream} for one upstream whose key counts or states changed; event: sessions and event: host carry {observed_at, sessions|host} with expose_host_stats; a `: heartbeat` comment comes every 15 s",
            "security": [{"peerAuth": []}],
            "responses": {
                "200": {"description": "OK", "content": {"text/event-stream": {"schema": {"type": "string"}}}},
                "401": {"description": "Missing or wrong peer token", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
                "429": {"description": "Too many failed logins, or max_subscribers_per_peer streams open from this address, or max_subscribers_per_token with this token", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
                "503": {"description": "max_subscribers streams are open", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ErrorEnvelope"}}}},
            },
        },
    });
    vec![
        (super::publisher::HEALTH_PATH.to_owned(), health),
        (super::stream::STREAM_PATH.to_owned(), stream),
    ]
}

#[cfg(test)]
mod tests {
    use super::provider_health_openapi;

    fn normalize(text: &str) -> String {
        text.replace(env!("CARGO_PKG_VERSION"), "<version>")
    }

    #[test]
    fn provider_health_openapi_matches_contract_doc() {
        let value = provider_health_openapi();
        let mut actual = serde_json::to_string_pretty(&value).unwrap();
        actual.push('\n');
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/provider-health.openapi.json");
        if std::env::var_os("REIKI_BLESS_GOLDEN").is_some() {
            std::fs::write(&path, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert_eq!(normalize(&actual), normalize(&expected));
    }
}
