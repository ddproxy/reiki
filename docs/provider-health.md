# Provider health contract

`GET /v1/health` on the peer listener returns a read-only JSON document describing key health for one Reiki instance. `GET /v1/health/stream` serves the same document as an SSE `snapshot`, then `key`, `host`, and `sessions` events plus `: heartbeat` comments. The machine-readable contract is [`provider-health.openapi.json`](provider-health.openapi.json), generated from the same code as the admin OpenAPI and checked by tests.

## Contract rules

- **Unknown fields are ignored.** Readers must accept documents with unknown object fields. Writers may add optional fields.
- **Enum-like values are extensible.** Readers must ignore unknown `state`, `last_status_class`, `status_class`, `cooldown_class`, and similar enum-like values rather than rejecting the document.
- **Optional means may be absent.** A field not listed as required may be omitted. Readers must not require optional fields.
- **Nullability is explicit.** Fields typed as `integer|null`, `string|null`, `array|null`, or `object|null` may be null. `soonest_cooldown_seconds` is an integer >= 1 or null; `0` is never published for a cooling wait.
- **Units are explicit.** Time durations named `*_seconds` are seconds. `*_at`/`observed_at` are RFC3339 UTC timestamps. `age_seconds` is the age of figures when sent; readers add their own time since receipt for freshness.
- **Version negotiation.** Readers should use capability/field presence, not strict version pinning. `reiki_version` identifies the publisher; unknown fields and enum-like values must remain readable across versions.

## Required top-level fields

`instance`, `reiki_version`, `observed_at`, `age_seconds`, `failures`, `upstreams`, `observed` are always present.

## Optional/non-core extensions

`gateway`, `streams`, `host`, `sessions`, `requestlog_export_dropped_total`, upstream usage arrays, upstream `keys`, and `capacity_group` are optional. `host` and `sessions` are non-core extensions, off by default, enabled only by `expose_host_stats` (and `sessions.streams` also needs `expose_key_labels`).

## Auth and errors

Requests use `Authorization: Bearer <peer token>` on the peer listener only. Peer tokens are read-only and must differ from admin and stream tokens. Auth/throttle failures return the same JSON error envelope shape as other Reiki listeners: `{"ok":false,"error":{"code":"...","message":"..."}}`.

## Operational notes

Peer publishing and reading configuration remain in [`configuration.md`](configuration.md), listener/deploy hardening in [`deploy.md`](deploy.md) and [`security.md`](security.md), and live fleet display in [`top.md`](top.md) and [`monitoring.md`](monitoring.md).
