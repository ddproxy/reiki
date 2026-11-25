# Monitoring

Reiki serves an admin listener on its own socket when an `[admin]` table exists. This page is the reference for its routes, the Prometheus metrics plugin and the alerts built from them. The client listener's `/__reiki/healthz` and the peer listener's `/v1/health` belong to other sockets; see [peers](peers.md) and [troubleshooting](troubleshooting.md).

## Configure and authenticate

```toml
[admin]
listen = "127.0.0.1:4098"
token_sha256 = "<sha256 of the admin token>"
read_token_sha256 = "<sha256 of the read-only token>"   # optional
```

`listen` and `token_sha256` are required. The address is bound at startup, so moving `[admin]` needs a restart; a changed token hash applies on reload. Generate each hash with `printf '%s' "$TOKEN" | reiki hash-token`. `reiki check` prints the admin address next to the client one.

Two credentials can open the listener:

| Token | Config key | Opens |
| --- | --- | --- |
| Full admin token | `token_sha256` | Every route below |
| Read-only token | `read_token_sha256` (optional) | Read routes only: `GET /v1/status`, `/v1/failures`, `/v1/stream-console`, `/v1/metrics`, `/v1/openapi.json` and the other `GET` plugin routes |

Startup refuses a read-only hash that equals the admin hash, a stream token, the stream-management token or a peer token, so a holder of the read-only token can never reach a mutating route. Each route checks the presented token with a constant-time comparison.

Every route below requires `Authorization: Bearer <token>`. A missing or wrong token gets `401`. A read-only token on a mutating route gets `403`. After ten bad tokens from one address in a minute, the next request gets `429` with `Retry-After`. Every admin response carries `X-Reiki-Version`. The built-in routes and plugin routes are served under `/v1`. An unknown path gets `404`. The listener has no TLS option: config validation refuses a non-loopback `listen`, so terminate TLS in front of a loopback bind for remote access.

### Error envelope

Every refusal on the admin listener, including `401`, `403`, `404`, `400`, `422`, `429` and `5xx`, has one JSON body:

```json
{"ok":false,"error":{"code":"read_only_token","message":"the read-only token cannot change state; this route needs the full admin token"}}
```

`code` is a stable snake_case identifier to branch on. `message` is text for people and may change between releases. Codes in use: `unauthorized` (missing or wrong token; the same body for both), `too_many_attempts`, `read_only_token`, `not_found`, `bad_request`, `bad_dry_run`, `unknown_key`, `config_refused`, `reload_task_failed`, `enable_task_failed`, `status_unavailable`, `failures_unavailable`, `audit_unavailable`, and the stream-management codes listed under [stream administration routes](#stream-administration-routes). Success bodies keep their own shapes.

### Read-only token

Give the read-only token to dashboards and `reiki top`. It gets the same data as the full token with two exceptions, both free text that can carry upstream or operator wording:

- `GET /v1/failures` rows have `"note": null`.
- `GET /v1/status` shows `config.last_reload.error` as `"reload_refused"` and `stream_admin.state_error` as `"state_unavailable"`.
- `GET /v1/stream-console` audit records have `"reason": null`.

`reiki top` reads `REIKI_ADMIN_READ_TOKEN` and falls back to `REIKI_ADMIN_TOKEN`. `reiki reload` and `reiki keys --enable` need the full token.

### Mutation audit

`POST /v1/reload` (including `dry_run`) and `POST /v1/keys/enable` each write one structured `tracing` event with target `reiki::admin_audit`, fields `action`, `token_class` (`full` or `read_only`), `actor` and `outcome`. A request may name itself with the `X-Reiki-Actor` header. The label is cut to 64 characters, and any character outside `A-Z a-z 0-9 . _ - @ :` becomes `_`. It is for reading logs only and never affects authorization. Refused requests, including a read-only token on these routes, are not audited as actions; they go to the warning log with the peer and path.

The `/v1/streams*` routes are the exception: they exist only with `[stream_admin]` and require the dedicated stream-management token instead of the admin token. See [stream administration routes](#stream-administration-routes). `GET /v1/stream-console` is not one of them: either admin token opens it, with or without stream administration.

```bash
export ADMIN_TOKEN='...'
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/status
```

## GET /v1/status

Method `GET`. Auth: bearer token. Query: none. Shows key health, stream chains and reload state.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/status | jq .
```

```json
{
  "keys": [
    {
      "name": "zen-a",
      "upstream": "zen",
      "consecutive_failures": 0,
      "last_status_code": 200,
      "state": "available",
      "scopes": []
    },
    {
      "name": "zen-b",
      "upstream": "zen",
      "consecutive_failures": 2,
      "last_status_code": 429,
      "state": "cooling",
      "retry_after_seconds": 42,
      "cooldown_class": "rate_limited",
      "scopes": [
        {
          "route": "/chat/completions",
          "model": "gpt-4o-mini",
          "retry_after_seconds": 60,
          "cooldown_class": "rate_limited"
        }
      ]
    }
  ],
  "streams": [
    {
      "name": "client-a",
      "strategy": "sticky",
      "chain": ["zen-a", "zen-b"],
      "current_key": "zen-a",
      "limits": {
        "rpm": 120,
        "tpm": null,
        "concurrent": null,
        "daily_requests": null,
        "daily_tokens": null,
        "monthly_requests": null,
        "monthly_tokens": null
      },
      "usage": {
        "requests": 42,
        "tokens": 2100,
        "minute_requests": 12,
        "minute_tokens": 600
      }
    }
  ],
  "config": {
    "generation": 3,
    "loaded_at": "2026-11-18T09:00:00Z",
    "last_reload": { "ok": true, "at": "2026-11-18T10:12:00Z" }
  },
  "observed": [
    {
      "upstream": "relay",
      "account": "primary",
      "mode": "observed",
      "status_class": "rate_limited",
      "observed_at": "2026-11-18T10:12:05Z",
      "retry_after_seconds": 42,
      "cooldown_class": "rate_limited",
      "quota": {
        "requests_limit": 100,
        "requests_remaining": 7,
        "requests_reset_seconds": 60,
        "tokens_limit": null,
        "tokens_remaining": null,
        "tokens_reset_seconds": null
      }
    }
  ],
  "peers": {
    "tls_ca": [
      { "file": "/etc/reiki/peers/peer-a-ca.pem", "ca_not_after": 1790000000, "ca_days_left": 120 }
    ]
  },
  "intercept": { "ca_not_after": 1790000000, "ca_days_left": 120 },
  "log": { "write_errors": 0 },
  "requestlog": { "export_dropped_total": 0 }
}
```

Key fields:

| Field | Meaning |
| --- | --- |
| `name`, `upstream` | Configured key name and its upstream |
| `state` | `available`, `cooling` or `out_of_rotation` |
| `consecutive_failures` | Transient failures since the last success |
| `last_status_code` | Latest upstream status, or `null` before an attempt |
| `retry_after_seconds` | Present while `cooling`, rounded up to at least 1 |
| `cooldown_class` | Present while cooling with a known class: `rate_limited`, `quota_exhausted` or `transient`; a restored cooldown has no class |
| `scopes` | Soonest-first route-and-model cooldowns, at most 16 entries; the key can stay `available` for other combinations |

`observed` always appears, with one entry per `(upstream, account)` pair from passthrough answers. Its `status_class` is one of `ok`, `rate_limited`, `quota_exhausted`, `credential_rejected`, `access_disabled`, `request_error`, `error` or `pass_through`; `mode` is always `observed`. `retry_after_seconds` counts down and becomes `null` when expired; `cooldown_class` is `null` when the answer started none. `quota` holds request and token limits, remaining counts and reset seconds read from response headers, with `null` for fields the upstream did not send. No credential value is held in the store. `peers.tls_ca` lists configured peer CA files with `file`, `ca_not_after` (Unix seconds) and `ca_days_left`; unreadable or non-certificate files are warned about and omitted.

Stream fields: `name`, `strategy` (`sticky` or `round_robin`), `chain`, `current_key` (or `null`), `limits` and `usage`. `limits` lists each configured numeric limit or `null`, plus `period_zone` when set; `usage` holds lifetime `requests` and `tokens` and the current `minute_requests` and `minute_tokens`. `config` holds `generation`, `loaded_at` and `last_reload`, which is `null` before any reload and otherwise has `ok`, `at` and an `error` after a failure. `intercept` appears with `[tunnel.intercept]` and holds `ca_not_after` in Unix seconds and `ca_days_left`. `log.write_errors` counts service-log file writes that failed and were dropped since start, `0` without a log file. `requestlog.export_dropped_total` counts summary records the request-log export could not queue or send; it is `0` unless `[plugins.requestlog.export]` is enabled. The response carries key names, never key values, token hashes or file paths.

`reiki keys` reads this route and prints a table; `reiki keys --json` prints a derived key-only document with remapped states and a `streams` name list per key. See [key health](troubleshooting.md#key-health).

## GET /v1/failures

Method `GET`. Auth: either token. Query: `limit` (1 to 64, default 64), `key` (one key name), `stream` (one stream name). Returns the in-memory ring, newest first. `note` is the scrubbed body snippet for the full token and `null` for the read-only token. A repeated flag, a bad `limit`, an empty name or invalid percent-encoding gets `400` with the [error envelope](#error-envelope).

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://127.0.0.1:4098/v1/failures?limit=2&stream=client-b&key=zen-b' | jq .
```

```json
[
  {
    "time": 1763465051,
    "stream": "client-b",
    "key": "zen-b",
    "status": 429,
    "class": "rate_limited",
    "action": "cooldown",
    "retry_after": "30",
    "cooled": true,
    "cooling_seconds": 60,
    "note": "rate limit exceeded"
  },
  {
    "time": 1763464980,
    "stream": "client-b",
    "key": "zen-b",
    "status": null,
    "class": "transient",
    "action": "pass-through",
    "retry_after": null,
    "cooled": false,
    "cooling_seconds": null,
    "note": "connection refused"
  }
]
```

| Field | Meaning |
| --- | --- |
| `time` | Unix seconds |
| `stream`, `key` | Where the failure happened |
| `status` | Upstream status, or `null` for a transport failure |
| `class` | `rate_limited`, `quota_exhausted`, `access_disabled`, `unauthorized`, `transient` or `request_error` |
| `action` | `cooldown` when this failure cooled a scope or the key, `rotate` when an `unauthorized` disabled the key, or `pass-through` when the key was only skipped for this request |
| `retry_after` | Received `Retry-After` header, cut to 64 bytes, or `null` |
| `cooled` | True when this failure started a cooldown or disabled the key |
| `cooling_seconds` | Seconds left on the key's cooldown, or `null` |
| `note` | Printable body snippet up to 200 bytes with credentials scrubbed |

The ring is in memory only: [`failure_ring_size`](configuration.md#top-level) sets its capacity, `200` by default, `0` disables it and the maximum is `10,000`. It starts empty after a restart and drops a record when it is full, disabled or another thread holds the buffer. A reload applies a smaller cap by dropping the oldest records. `limit` stays capped at `64` regardless of capacity. `reiki failures --limit 20` prints this data as a table; `--json` prints the array. The [worked example](troubleshooting.md#worked-example-quota-rate-limit-or-request-size) reads those rows.

## GET /v1/stream-console

Method `GET`. Auth: either admin token. Query: none. Read-only snapshot of every configured stream for the Streams screen of [`reiki top`](top.md#streams). It exists with or without `[stream_admin]`; the stream-management token is refused. Nothing here changes a stream. For the read-only token the audit `reason` fields are `null`.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" \
  http://127.0.0.1:4098/v1/stream-console | jq .
```

```json
{
  "enabled": true,
  "streams": [
    {
      "name": "client-a",
      "dynamic": true,
      "chain": ["zen-a", "zen-b"],
      "limits": { "rpm": 120, "tpm": null, "concurrent": 4 },
      "tokens": [
        { "id": "<token id>", "expires_at": 1790000000, "state": "active" }
      ],
      "expiry": 1790000000,
      "rpm": { "current": 12, "limit": 120 },
      "concurrency": { "current": 1, "limit": 4 },
      "last_used": 1763465051,
      "state": "active"
    }
  ],
  "audit": [
    {
      "seq": 2,
      "time": "2026-11-23T00:01:00Z",
      "action": "token.add",
      "stream": "client-a",
      "outcome": "success",
      "reason": "rotation",
      "changes": [{ "field": "token.id", "from": "-", "to": "<first 8 of id>" }]
    }
  ]
}
```

`enabled` is false when `[stream_admin]` is absent; static streams are still listed and `audit` is empty. `tokens` carries only an id, expiry and derived state, never a credential value or hash. `rpm.current` is admissions in the current fixed 60-second window, `concurrency.current` is in-flight requests, and `last_used` is the Unix time of the last admission in this process (`null` before one). `expiry` is the soonest live token expiry, and `state` is `active`, `expired` or `revoked`. Audit records are newest first, capped at 64, and carry only `seq`, `time`, `action`, `stream`, `outcome`, `reason` and field-level `changes`; the actor hash, source, `prev`, `hash` and raw log path are omitted. A corrupt audit chain answers `500` instead of returning partial records. See [keys](keys.md#dynamic-stream-credentials) for the audit log itself.

## POST /v1/keys/enable

Method `POST`. Auth: full admin token; the read-only token gets `403`. Query: `key` (required, percent-encoded key name). Clears the key's cooldown, disabled flag, consecutive failures and combination cooldowns, and saves the change so a restart keeps it. Writes an [audit event](#mutation-audit).

```bash
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://127.0.0.1:4098/v1/keys/enable?key=zen-a'
```

```json
{"ok":true,"key":"zen-a"}
```

| Status | Response |
| --- | --- |
| `200` | `{"ok":true,"key":"<name>"}`; also for a key that was available |
| `400` | Missing `key`, a repeated or bare `key` flag, an unknown flag, or a name with invalid percent-encoding (`bad_request`) |
| `401` | Missing or wrong token (`unauthorized`) |
| `403` | The read-only token (`read_only_token`) |
| `404` | No configured key with that name (`unknown_key`) |

The CLI equivalent is `reiki keys --enable zen-a --token-file /etc/reiki/admin.token`.

## POST /v1/reload

Method `POST`. Auth: full admin token; the read-only token gets `403`, even for a preview. Query: `dry_run` with `true`, `1`, `false` or `0`; anything else gets `400` (`bad_dry_run`) and applies nothing. Writes an [audit event](#mutation-audit). Re-reads the config, key files and CA files, then swaps the new state in when every part validates.

```bash
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://127.0.0.1:4098/v1/reload?dry_run=true' | jq .
```

```json
{"ok":true,"dry_run":true,"generation":3,"keys":2,"streams":1,"changed":["plugins phi","tunnel allow +2/-0"],"restart_needed":[]}
```

A successful apply has the same fields with `"dry_run":false`; `changed` lists what differs and `restart_needed` lists settings that wait for a restart. A config that fails validation gets `422` with error code `config_refused`, and its message names the problem with the file, never a value from it; a failed task gets `500` (`reload_task_failed`). `reiki reload --dry-run` calls this route and prints the JSON answer. A preview leaves `/v1/status` unchanged. See [reload](troubleshooting.md#reload) for the live-change list.

## Stream administration routes

With `[stream_admin]` configured, the admin listener also serves these routes. All of them require `Authorization: Bearer <stream-admin token>`: the ordinary admin token and a stream token are refused. There is no HTTP route for the raw audit log file; the ordinary-token [`GET /v1/stream-console`](#get-v1stream-console) exposes a bounded, redacted tail for the TUI.

| Route | Purpose |
| --- | --- |
| `GET /v1/streams` | List dynamic streams. |
| `GET /v1/streams/{name}` | Show one dynamic stream. |
| `PUT /v1/streams/{name}` | Change a dynamic stream's assignable key or limits. |
| `DELETE /v1/streams/{name}` | Delete a dynamic stream. |
| `POST /v1/streams/create` | Create a dynamic stream and return its first token once. |
| `POST /v1/streams/{name}/tokens` | Add a token and return it once. |
| `POST /v1/streams/{name}/tokens/{token_id}/revoke` | Revoke one token. |
| `POST /v1/streams/{name}/revoke` | Revoke every token on a stream. |

Every write needs an `Idempotency-Key` header and a `reason` in the JSON body; strict JSON parsing rejects unknown fields, and a configured second party also requires `X-Confirm-Token`. Keys must be listed in `[stream_admin.allowed_keys]` with `assignable = true`, and limits must stay within `[stream_admin.ceilings]`. Failed attempts are throttled per source, and a failed audit write refuses the mutation. The [route list and CLI examples](README.md#stream-admin-api) cover the `reiki streams` commands. With the feature on, `GET /v1/status` adds a `stream_admin` object with `enabled`, `streams` and `state_error`.

## GET /v1/openapi.json

Method `GET`. Auth: bearer token. Query: none. Returns the OpenAPI 3.1 document built from the route table. While publishing, it also lists the peer listener's `/v1/health` and `/v1/health/stream` with that listener as their server. The `/v1/streams*` paths and the `streamAdminAuth` scheme appear only when `[stream_admin]` is configured.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/openapi.json \
  | jq -r '.info, (.paths | keys[])'
```

```text
{"title":"Reiki admin API","version":"0.14.0"}
/v1/failures
/v1/keys/enable
/v1/metrics
/v1/openapi.json
/v1/reload
/v1/status
/v1/stream-console
/v1/usage
```

## Other routes

| Route | Location | Auth | Purpose |
| --- | --- | --- | --- |
| `GET /__reiki/healthz` | Client listener | None | Returns `200` with body `ok` while Reiki is up |
| `GET /v1/usage` | Client listener | Stream token | JSON request and token totals for that stream only |
| `GET /v1/metrics` | Admin listener, `metrics` plugin | Either admin token | Prometheus text exposition |
| `GET /v1/requestlog/metrics` | Admin listener, `requestlog` plugin | Either admin token | Prometheus summary-export drop counter |
| `GET /v1/toolpolicy/metrics` | Admin listener, `toolpolicy` plugin | Either admin token | Prometheus tool-call counters |
| `GET /v1/usage` | Admin listener, `metrics` plugin | Either admin token | JSON token totals per stream and upstream |
| `GET /v1/stream-console` | Admin listener | Either admin token | Read-only stream table and redacted audit tail for `reiki top` |
| `GET /v1/cost` | Admin listener, `cost` plugin | Either admin token | Estimated spend with period, stream and upstream filters |
| `GET /v1/cost/metrics` | Admin listener, `cost` plugin | Either admin token | Cost estimate metrics in Prometheus text |
| `GET /v1/health` | Peer listener | Peer read token | Coarse health of this instance for other Reikis |
| `GET /v1/health/stream` | Peer listener | Peer read token | Server-sent changes to that health document |

The plugin routes exist only when their plugin is configured: `/v1/metrics` needs `[plugins.metrics]`, `/v1/toolpolicy/metrics` needs `[plugins.toolpolicy]`, `/v1/requestlog/metrics` needs `[plugins.requestlog]`, and `/v1/cost` and `/v1/cost/metrics` need `[plugins.cost]` plus the `plugin-cost` build feature. An unknown admin path gets `404`, and a plugin route with no plugin answers an empty `404`. Peer routes use the `[peers]` token and belong to the publishing side described in [peers publishing](peers-publishing.md).

## GET /v1/metrics

Served by the `metrics` plugin with the same bearer auth as `/v1/status`. Without `[plugins.metrics]` the route returns `404`.

```toml
[plugins.metrics]
```

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/metrics
```

```text
# HELP reiki_requests_total Proxied requests by stream, upstream and response class.
# TYPE reiki_requests_total counter
reiki_requests_total{stream="client-a",upstream="zen",status="success"} 42
# HELP reiki_keys Keys by rotation state.
# TYPE reiki_keys gauge
reiki_keys{state="available"} 1
reiki_keys{state="cooling"} 1
reiki_keys{state="out_of_rotation"} 0
```

The content type is `text/plain; version=0.0.4; charset=utf-8`. Counters survive a reload in memory; series for renamed streams, upstreams and keys stay until restart.

| Metric | Type | Labels | Counts |
| --- | --- | --- | --- |
| `reiki_requests_total` | counter | `stream`, `upstream`, `status` | Requests once each, by client-visible class and serving upstream; a client that disconnects first counts as `client_error` |
| `reiki_failovers_total` | counter | `stream`, `key`, `class` | Key failures handled by trying another key |
| `reiki_cooldowns_total` | counter | `key`, `class`, `scope` | Keys put on cooldown or out of rotation; `scope` is `key` or `model` |
| `reiki_rate_limit_rejections_total` | counter | `stream` | Requests rejected when every key was shaped out locally (`reiki_shaped`) |
| `reiki_gateway_rejections_total` | counter | `stream`, `reason` | Requests refused by stream limits or allowlists; `reason` is a fixed label such as `rate`, `tokens`, `model`, `model_unknown`, `concurrent`, `quota_requests`, `quota_tokens`, `fairness` or `store_closed` |
| `reiki_gateway_store_errors_total` | counter | `stream` | Counter-store failures while serving in `open` mode |
| `reiki_gateway_store_ok` | gauge | none | `1` while counter state is readable and writable, `0` while the gateway store is closed |
| `reiki_failure_ring_entries` | gauge | none | Recent upstream failures currently held in the in-memory ring |
| `reiki_failure_ring_dropped_total` | counter | none | Failures not retained because the ring was full, disabled or contended |
| `reiki_plugin_rejections_total` | counter | `plugin`, `stream` | Plugin rejections, plus skips and panics under `on_error = "reject"` |
| `reiki_findings_total` | counter | `stream`, `plugin`, `kind`, `action` | Identifiers found in bodies by `phi`, `finance` or `controlled`; `action` is `blocked` or `reported` |
| `reiki_tokens_total` | counter | `stream`, `upstream`, `direction` | Input and output tokens reported by providers |
| `reiki_keys` | gauge | `state` | Keys in `available`, `cooling` and `out_of_rotation`, always all three series |
| `reiki_upstream_requests_1h` | gauge | `upstream` | Attempts in the last hour |
| `reiki_upstream_limited_1h` | gauge | `upstream` | Attempts in the last hour that were rate-limited or began a cooldown |
| `reiki_upstream_tokens_1h` | gauge | `upstream` | Input plus output tokens in the last hour |
| `reiki_upstream_unaccounted_1h` | gauge | `upstream` | Attempts in the last hour with no readable usage figures |
| `reiki_cost_estimated_spend_micros` | gauge | `stream`, `upstream`, `currency` | Estimated spend in integer micro-units; only with the `cost` plugin |
| `reiki_cost_requests_total` | gauge | `stream`, `upstream`, `currency` | Requests with a recorded cost row; only with the `cost` plugin |
| `reiki_cost_unpriced_requests_total` | gauge | `stream`, `upstream`, `currency` | Requests whose model had no price; only with the `cost` plugin |
| `reiki_cost_model_spend_micros` | gauge | `stream`, `upstream`, `currency`, `model` | Spend for an allowlisted model; only with `model_label_allowlist` |
| `reiki_passthrough_requests_total` | counter | `upstream`, `status_class` | Passthrough answers by fixed observed class; no credential, account or key label is exported |
| `reiki_observed_cooldown_seconds` | gauge | `upstream`, `class` | Longest remaining informational cooldown across that upstream's observed accounts; `0` when none |
| `reiki_stream_admin_actions_total` | counter | `result` | Management actions by result, including denied and failed attempts; only with `[stream_admin]` |
| `reiki_dynamic_streams` | gauge | none | Active dynamic streams; only with `[stream_admin]` |
| `reiki_stream_tokens_expiring_soon` | gauge | none | Unexpired dynamic stream tokens expiring within one hour; only with `[stream_admin]` |

`status` is `success` for `100`–`399`, `client_error` for `400`–`499` and `server_error` for the rest. `class` uses the names in [response classification](classification.md#response-rules). `reiki_findings_total` keeps a plugin's `detail` label out of its series and never holds a matched identifier value. The four `reiki_upstream_*_1h` gauges are sampled from the same counters behind `GET /v1/health`: they age out, restart at zero with the process, and appear while usage exposure is on (`expose_usage` defaults to true, including when no `[peers]` table exists). The observed series come from the in-memory observation store; `account` stays out of metrics to keep cardinality bounded by upstream and class. `reiki_gateway_store_ok` and the two `reiki_failure_ring_*` series are appended by the host on every scrape, so they appear even without plugin routes for store failures or ring metrics. The three stream-management series are appended to this same route when `[stream_admin]` is configured and are absent otherwise. The cost plugin's series are appended when `[plugins.cost]` is active. Their `stream` and `upstream` labels are capped at 64 bytes, at most 64 series are emitted, and a `model` label appears only for names in `model_label_allowlist`; no price, token or request value is ever a label.

## GET /v1/toolpolicy/metrics

Served by the `toolpolicy` plugin with the same bearer auth. Without `[plugins.toolpolicy]` the route returns `404`. See [tool policy](toolpolicy.md#metrics) for the settings and a rollout sequence.

| Metric | Type | Labels | Counts |
| --- | --- | --- | --- |
| `reiki_toolpolicy_calls_total` | counter | `stream`, `upstream`, `action` | Tool calls seen in responses; `action` is `passed`, `reported` or `blocked` |
| `reiki_toolpolicy_findings_total` | counter | `rule`, `severity` | Rule matches, including findings below `block_severity` |
| `reiki_toolpolicy_unparsed_total` | counter | `stream`, `upstream` | Recognized responses that could not be parsed |

## GET /v1/requestlog/metrics

Served by the `requestlog` plugin with the same bearer auth. Without `[plugins.requestlog]` the route returns `404`. The counter covers [summary export](requestlog.md#summary-export) only; drops from the main request log queue are recorded as a `dropped` record instead.

| Metric | Type | Labels | Counts |
| --- | --- | --- | --- |
| `reiki_requestlog_export_dropped_total` | counter | none | Summaries dropped because the export queue was full or a syslog send failed |

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/requestlog/metrics
```

The same value is published as `requestlog.export_dropped_total` in `GET /v1/status` and as `requestlog_export_dropped_total` in `/v1/health`, which `reiki top` shows when it is above zero.

## GET /v1/usage (client listener)

Method `GET`. Auth: this stream's bearer token, the same one used for proxy requests. Query: none. A stream sees only its own row; there is no admin token and no way to ask for another stream. The route has its own `[gateway] usage_rpm` budget (default 60 per minute): over it the response is `429` `gateway_usage_limited` with `Retry-After`. A missing or unknown token gets `401`.

```bash
curl -s -H "Authorization: Bearer $STREAM_TOKEN" http://127.0.0.1:4097/v1/usage
```

```json
{"streams":[{"name":"client-a","requests":42,"tokens":2100,"minute_requests":12,"minute_tokens":600,"cost":{"currency":"USD","period":"2026-11","estimated_micros":"12500000","estimated_display":"12.500000","unpriced_requests":0,"note":"Estimates from the operator price table, not provider invoices."}}]}
```

`requests` and `tokens` are lifetime totals from admitted requests; `tokens` uses the provider's measured usage when present and the request-size estimate otherwise. The minute values come from the current fixed 60-second window. The `cost` object appears only when the `cost` plugin is active, and only on the authenticated caller's row; see [cost](cost.md#self-service-get-v1usage). The body contains only configured stream names and counts, never token hashes, key values, upstream credentials or model names. This route is separate from the admin plugin route below.

## GET /v1/usage (admin listener)

Method `GET`. Auth: bearer token. Query: none. Same plugin as `/v1/metrics`. Totals come from provider `usage` objects: `prompt_tokens`/`completion_tokens` or `input_tokens`/`output_tokens`; event streams combine the events that carry `usage`. Bodies over 1 MiB and compressed responses count nothing.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/usage
```

```json
{"stream":{"upstream":{"input":10,"output":20,"requests":1}}}
```

## GET /v1/cost and GET /v1/cost/metrics

Served by the `cost` plugin with the same bearer auth as `/v1/status`. Both routes return `404` without `[plugins.cost]`, and the plugin is off by default and needs the `plugin-cost` build feature. `/v1/cost` takes the strict query filters `period`, `stream`, `upstream` and `format` (`json` or `csv`). `/v1/cost/metrics` returns the bounded cost series appended to `/v1/metrics`; it takes no query. When the plugin is active, `GET /v1/openapi.json` describes both routes with their filters and a response note that the figures are estimates; `/v1/status` gains a `cost` object for the current UTC month. See [cost](cost.md#read-the-figures) for the worked example, CSV columns and budgets.

## Scrape configuration

```yaml
scrape_configs:
  - job_name: reiki
    metrics_path: /v1/metrics
    authorization:
      credentials_file: /etc/reiki/admin.token
    static_configs:
      - targets: ["127.0.0.1:4098"]
```

Prometheus sends the file's contents as the bearer credential. Keep the file readable only by the scrape process.

## Alert rules

```yaml
groups:
  - name: reiki
    rules:
      - alert: ReikiAllKeysCooling
        expr: |
          sum(reiki_keys{state="available"}) == 0
          and sum(reiki_keys) > 0
        for: 5m
        annotations:
          summary: Every Reiki key is cooling or out of rotation

      - alert: ReikiInterceptCANearExpiry
        expr: reiki_status_intercept_ca_days_left < 30
        for: 1h
        annotations:
          summary: Reiki's interception CA expires within 30 days

      - alert: ReikiStreamHasNoHealthyKey
        expr: reiki_stream_healthy_keys == 0
        for: 5m
        annotations:
          summary: A Reiki stream chain has no available key

      - alert: ReikiGatewayStoreClosed
        expr: reiki_gateway_store_ok == 0
        for: 1m
        annotations:
          summary: Reiki's per-stream counter store has been closed for over a minute
```

Reiki publishes `reiki_keys` globally, with no `stream` label. The two `reiki_status_*` and `reiki_stream_healthy_keys` series come from a JSON exporter that reads `GET /v1/status` with the admin token: `reiki_status_intercept_ca_days_left` from `intercept.ca_days_left`, and `reiki_stream_healthy_keys{stream="..."}` from the keys named in each `streams[].chain` whose `state` is `available`. Those names are exporter series, not built-in Reiki metrics. Reiki logs a CA warning within 30 days of expiry and reports `ca_days_left` on `/v1/status`; see [tunnel](tunnel.md#interception).

`reiki_gateway_store_ok` is appended by Reiki itself to every `/v1/metrics` scrape, so the alert fires even when no stream has sent traffic. `0` means per-stream counters are unavailable; with `[gateway] on_store_error = "closed"` (the default), new admissions get `503 gateway_store_unavailable`. A store closed by a failed state write probes itself on a timer, while one closed at startup by an unreadable state file needs the file repaired; see [operations](operations.md#troubleshoot-gateway-store-unavailable).

When `[peers] expose_usage` is on (the default), peer health publishes `streams.static_streams` and `streams.dynamic_streams` as counts only. `reiki top` sums fresh peers into a `Streams: N static, M dynamic` line in its full text view; it never shows stream names from peers, since a dynamic name can identify a tenant. Pressing `t` in the live view is different: it uses the local admin token to read [`GET /v1/stream-console`](#get-v1stream-console) and shows this instance's stream names, ids and redacted audit tail only. See [reiki top](top.md#streams).
