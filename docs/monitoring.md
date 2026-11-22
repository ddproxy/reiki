# Monitoring

Reiki serves an admin listener on its own socket when an `[admin]` table exists. This page is the reference for its routes, the Prometheus metrics plugin and the alerts built from them. The client listener's `/__reiki/healthz` and the peer listener's `/v1/health` belong to other sockets; see [peers](peers.md) and [troubleshooting](troubleshooting.md).

## Configure and authenticate

```toml
[admin]
listen = "127.0.0.1:4098"
token_sha256 = "<sha256 of the admin token>"
```

Both fields are required. The address is bound at startup, so moving `[admin]` needs a restart; a changed token hash applies on reload. Generate the hash with `printf '%s' "$ADMIN_TOKEN" | reiki hash-token`. `reiki check` prints the admin address next to the client one.

Every route below requires `Authorization: Bearer <admin token>`. The token hash is compared in constant time. A missing or wrong token gets a bare `401`. After ten bad tokens from one address in a minute, the next request gets `429` with `Retry-After`. Every admin response carries `X-Reiki-Version`. The built-in routes and plugin routes are served under `/v1`. An unknown path gets `404`. The listener has no TLS option: config validation refuses a non-loopback `listen`, so terminate TLS in front of a loopback bind for remote access.

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
      "state": "available"
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
      "rate_limit_per_minute": 120
    }
  ],
  "config": {
    "generation": 3,
    "loaded_at": "2026-11-18T09:00:00Z",
    "last_reload": { "ok": true, "at": "2026-11-18T10:12:00Z" }
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

Stream fields: `name`, `strategy` (`sticky` or `round_robin`), `chain`, `current_key` (or `null`) and `rate_limit_per_minute` (or `null`). `config` holds `generation`, `loaded_at` and `last_reload`, which is `null` before any reload and otherwise has `ok`, `at` and an `error` after a failure. `intercept` appears with `[tunnel.intercept]` and holds `ca_not_after` in Unix seconds and `ca_days_left`. `log.write_errors` counts service-log file writes that failed and were dropped since start, `0` without a log file. `requestlog.export_dropped_total` counts summary records the request-log export could not queue or send; it is `0` unless `[plugins.requestlog.export]` is enabled. The response carries key names, never key values, token hashes or file paths.

`reiki keys` reads this route and prints a table; `reiki keys --json` prints a derived key-only document with remapped states and a `streams` name list per key. See [key health](troubleshooting.md#key-health).

## GET /v1/failures

Method `GET`. Auth: bearer token. Query: `limit` (1 to 64, default 64), `key` (one key name). Returns the in-memory ring, newest first. A repeated flag, a bad `limit`, an empty key or invalid percent-encoding gets `400` with `{"ok":false,"error":"..."}`.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://127.0.0.1:4098/v1/failures?limit=2&key=zen-b' | jq .
```

```json
[
  {
    "time": 1763465051,
    "stream": "client-b",
    "key": "zen-b",
    "status": 429,
    "class": "rate_limited",
    "retry_after": "30",
    "cooled": true,
    "cooling_seconds": 60,
    "note": "rate limit exceeded"
  },
  {
    "time": 1763464980,
    "stream": "client-a",
    "key": "zen-b",
    "status": null,
    "class": "transient",
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
| `retry_after` | Received `Retry-After` header, cut to 64 characters, or `null` |
| `cooled` | True when this failure started a cooldown or disabled the key |
| `cooling_seconds` | Seconds left on the key's cooldown, or `null` |
| `note` | Printable body snippet up to 200 characters with credentials scrubbed |

The ring holds 64 entries, starts empty after a restart, and drops a record when another thread holds the buffer. `reiki failures --limit 20` prints this data as a table; `--json` prints the array. The [worked example](troubleshooting.md#worked-example-quota-rate-limit-or-request-size) reads those rows.

## POST /v1/keys/enable

Method `POST`. Auth: bearer token. Query: `key` (required, percent-encoded key name). Clears the key's cooldown, disabled flag, consecutive failures and combination cooldowns, and saves the change so a restart keeps it.

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
| `400` | Missing `key`, a repeated or bare `key` flag, an unknown flag, or a name with invalid percent-encoding |
| `401` | Missing or wrong token |
| `404` | No configured key with that name |

The CLI equivalent is `reiki keys --enable zen-a --token-file /etc/reiki/admin.token`.

## POST /v1/reload

Method `POST`. Auth: bearer token. Query: `dry_run` with `true`, `1`, `false` or `0`; anything else gets `400` and applies nothing. Re-reads the config, key files and CA files, then swaps the new state in when every part validates.

```bash
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  'http://127.0.0.1:4098/v1/reload?dry_run=true' | jq .
```

```json
{"ok":true,"dry_run":true,"generation":3,"keys":2,"streams":1,"changed":["plugins phi","tunnel allow +2/-0"],"restart_needed":[]}
```

A successful apply has the same fields with `"dry_run":false`; `changed` lists what differs and `restart_needed` lists settings that wait for a restart. A config that fails validation gets `422` with `{"ok":false,"error":"..."}`; a failed task gets `500`. `reiki reload --dry-run` calls this route and prints the JSON answer. A preview leaves `/v1/status` unchanged. See [reload](troubleshooting.md#reload) for the live-change list.

## GET /v1/openapi.json

Method `GET`. Auth: bearer token. Query: none. Returns the OpenAPI 3.1 document built from the route table. While publishing, it also lists the peer listener's `/v1/health` and `/v1/health/stream` with that listener as their server.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/openapi.json \
  | jq -r '.info, (.paths | keys[])'
```

```text
{"title":"Reiki admin API","version":"0.11.0"}
/v1/failures
/v1/keys/enable
/v1/metrics
/v1/openapi.json
/v1/reload
/v1/status
/v1/usage
```

## Other routes

| Route | Location | Auth | Purpose |
| --- | --- | --- | --- |
| `GET /__reiki/healthz` | Client listener | None | Returns `200` with body `ok` while Reiki is up |
| `GET /v1/metrics` | Admin listener, `metrics` plugin | Bearer token | Prometheus text exposition |
| `GET /v1/requestlog/metrics` | Admin listener, `requestlog` plugin | Bearer token | Prometheus summary-export drop counter |
| `GET /v1/toolpolicy/metrics` | Admin listener, `toolpolicy` plugin | Bearer token | Prometheus tool-call counters |
| `GET /v1/usage` | Admin listener, `metrics` plugin | Bearer token | JSON token totals per stream and upstream |
| `GET /v1/health` | Peer listener | Peer read token | Coarse health of this instance for other Reikis |
| `GET /v1/health/stream` | Peer listener | Peer read token | Server-sent changes to that health document |

The plugin routes exist only when their plugin is configured: `/v1/metrics` needs `[plugins.metrics]`, `/v1/toolpolicy/metrics` needs `[plugins.toolpolicy]`, and `/v1/requestlog/metrics` needs `[plugins.requestlog]`. An unknown admin path gets `404`, and a plugin route with no plugin answers an empty `404`. Peer routes use the `[peers]` token and belong to the publishing side described in [peers publishing](peers-publishing.md).

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
| `reiki_rate_limit_rejections_total` | counter | `stream` | Requests rejected by the stream's own rate limit |
| `reiki_plugin_rejections_total` | counter | `plugin`, `stream` | Plugin rejections, plus skips and panics under `on_error = "reject"` |
| `reiki_findings_total` | counter | `stream`, `plugin`, `kind`, `action` | Identifiers found in bodies by `phi`, `finance` or `controlled`; `action` is `blocked` or `reported` |
| `reiki_tokens_total` | counter | `stream`, `upstream`, `direction` | Input and output tokens reported by providers |
| `reiki_keys` | gauge | `state` | Keys in `available`, `cooling` and `out_of_rotation`, always all three series |
| `reiki_upstream_requests_1h` | gauge | `upstream` | Attempts in the last hour |
| `reiki_upstream_limited_1h` | gauge | `upstream` | Attempts in the last hour that were rate-limited or began a cooldown |
| `reiki_upstream_tokens_1h` | gauge | `upstream` | Input plus output tokens in the last hour |

`status` is `success` for `100`–`399`, `client_error` for `400`–`499` and `server_error` for the rest. `class` uses the names in [response classification](classification.md#response-rules). `reiki_findings_total` keeps a plugin's `detail` label out of its series and never holds a matched identifier value. The three `reiki_upstream_*_1h` gauges are sampled from the same counters behind `GET /v1/health`: they age out, restart at zero with the process, and appear while usage exposure is on (`expose_usage` defaults to true, including when no `[peers]` table exists).

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

## GET /v1/usage

Method `GET`. Auth: bearer token. Query: none. Same plugin as `/v1/metrics`. Totals come from provider `usage` objects: `prompt_tokens`/`completion_tokens` or `input_tokens`/`output_tokens`; event streams combine the events that carry `usage`. Bodies over 1 MiB and compressed responses count nothing.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/usage
```

```json
{"stream":{"upstream":{"input":10,"output":20,"requests":1}}}
```

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
```

Reiki publishes `reiki_keys` globally, with no `stream` label. The two `reiki_status_*` and `reiki_stream_healthy_keys` series come from a JSON exporter that reads `GET /v1/status` with the admin token: `reiki_status_intercept_ca_days_left` from `intercept.ca_days_left`, and `reiki_stream_healthy_keys{stream="..."}` from the keys named in each `streams[].chain` whose `state` is `available`. Those names are exporter series, not built-in Reiki metrics. Reiki logs a CA warning within 30 days of expiry and reports `ca_days_left` on `/v1/status`; see [tunnel](tunnel.md#interception).
