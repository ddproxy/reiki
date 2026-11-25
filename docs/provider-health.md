# Provider health contract

`GET /v1/health` on the peer listener returns a read-only JSON document describing key health for one Reiki instance. `GET /v1/health/stream` serves the same document as an SSE snapshot followed by updates. Peer publishing and tokens are configured in [configuration](configuration.md); listener hardening is in [security](security.md).

## Required fields

`instance`, `reiki_version`, `observed_at`, `age_seconds`, `failures`, `upstreams` and `observed` are always present.

Readers must accept unknown object fields and ignore unknown enum-like values such as `state`, `last_status_class`, `status_class` and `cooldown_class`. Optional fields may be absent or null; bounded arrays carry a `*_truncated` flag when entries were omitted. Durations named `*_seconds` are integer seconds, instants named `*_at` are RFC3339 UTC strings, and `age_seconds` is the age of the figures when sent. `cooldown_seconds` is the remaining wait for one key or observed upstream; `soonest_cooldown_seconds` is the soonest wait across a key set and is an integer >= 1 or null. Use field presence rather than strict version pinning.

## Auth and errors

Requests use `Authorization: Bearer <peer token>` on the peer listener only. Peer tokens are read-only and must differ from admin and stream tokens. Auth and throttle failures use the standard error envelope: `{"ok":false,"error":{"code":"...","message":"..."}}`.

## Fleet view

See [reiki top](top.md) for the live fleet display built from peer health documents.
