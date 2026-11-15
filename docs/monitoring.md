# Monitoring

Reiki can serve a small admin listener with `GET /status` and any plugin-provided path such as `GET /metrics`; it is off unless an `[admin]` table exists and always uses its own socket, never the client one.

## Admin API

Routes: `GET /status`, `POST /reload`, `POST /keys/enable`, `GET /metrics` (when provided by a plugin), and all of the above under `/v1/...` with `GET /v1/openapi.json`. All admin routes require `Authorization: Bearer <admin token>`. Admin responses include `X-Reiki-Version`. The v1 API only gains fields.

`GET /v1/health` and `GET /v1/health/stream`, the coarse summary other instances read, are never served here: they live on the separate `[peers]` listener with its own read-only token (see [peers](peers.md)). While publishing, `/v1/openapi.json` lists them with that listener as their server.

## Configure

```toml
[admin]
listen = "127.0.0.1:4098"                 # must differ from the client `listen`
token_sha256 = "<sha256 of the admin token>"
```

Both fields are required. The admin address is bound at startup, so adding, removing or moving `[admin]` needs a restart; a changed token hash applies on reload. Generate the hash with `printf '%s' "$ADMIN_TOKEN" | reiki hash-token`; `reiki check` prints the admin address alongside the client one.

## Authenticate

Send `Authorization: Bearer <token>`. A missing or wrong token gets a bare 401 with no body; the token is hashed and compared in constant time. Anything other than `/status`, `/reload`, `/keys/enable` or a configured plugin path returns 404. The listener is plain HTTP, so keep it on loopback or put a TLS-terminating proxy in front.

## GET /status

With `[tunnel.intercept]`, the document also has `intercept.ca_not_after` (Unix seconds) and `intercept.ca_days_left`, so an expiring CA can be alerted on. It is JSON with `keys` and `streams` arrays.

Each key:

- `name`, `upstream`
- `state`: `available`, `cooling` or `out_of_rotation`
- `retry_after_seconds`: present only while `cooling`
- `cooldown_class`: present only while `cooling` and known: `rate_limited`, `quota_exhausted` or `transient`, the failure that set the deadline (same names as `reiki_cooldowns_total`). A restored cooldown has no class until the key fails again.
- `scopes`: present only while route-and-model combinations are cooling; soonest first, at most 16. Each has `route`, `model`, `retry_after_seconds` and `cooldown_class`; the key may still be `available` for others.
- `consecutive_failures`: consecutive transient failures since the last success
- `last_status_code`: the upstream status of the key's latest attempt, or `null` before one

Each stream:

- `name`, `strategy` (`sticky` or `round_robin`), `chain`
- `current_key`: the key the stream would prefer next, or `null`
- `rate_limit_per_minute`: the stream's limit, or `null`

- `config`: `generation` (successful reloads since start), `loaded_at` and `last_reload`, which is `null` before any reload and otherwise has `ok`, `at` and, after a failure, `error`

The response carries no key values, token hashes or file paths.

## Check key health

`reiki keys` calls `GET /status` and prints one row per key: its name, upstream, state (`ok`, `cooling` or `disabled`), cooldown seconds remaining, last status code and the streams whose chain includes it. `reiki keys --json` prints a JSON document with a `keys` array instead, carrying each key's `scopes` array when it has cooling combinations. The table itself does not show scopes. `reiki keys --enable <name>` calls `POST /keys/enable` to re-enable a key that was taken out of rotation.

```bash
reiki keys --token-file /etc/reiki/admin.token
reiki keys --url http://127.0.0.1:4098 --token-file /etc/reiki/admin.token --json
```

It takes `--url` and `--token-file` like `reiki reload`, falling back to `[admin] listen` and `REIKI_ADMIN_TOKEN`. Exit status: 0 on success, 1 when the instance refuses the token or answer, 2 when it cannot reach the instance or resolve the admin address. The same flags apply to `--enable`, which conflicts with `--check` and `--json`.

To check the key files from the config without a running instance, use `reiki keys --check`; see [troubleshooting](troubleshooting.md#key-health).

## POST /keys/enable

Re-enables a key that was taken out of rotation, so its chain may pick it again. The key's name is sent as `?key=<name>`, percent-encoded. Enabling clears the key's cooldown, its disabled flag and its consecutive-failure count, and persists the change like a reload does, so a restart does not undo it.

```bash
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" \
  "http://127.0.0.1:4098/v1/keys/enable?key=zen-a"
```

```bash
reiki keys --enable zen-a --token-file /etc/reiki/admin.token
```

Responses:

- `200`: `{"ok":true,"key":"<name>"}`.
- `400`: no `key` name given, or a name that is not valid percent-encoding. Nothing changed.
- `401`: missing or wrong token, as everywhere on the admin listener.
- `404`: no key with that name is configured. Nothing changed.

A key that was never out of rotation answers `200` and stays available; the endpoint only clears its cooldown. [Key health](troubleshooting.md#key-health) explains how keys are taken out and what `cooling` means.

## POST /reload

Applies a changed config like `SIGHUP`, with the same bearer token as `/status`, and returns JSON. Success answers `200`:

```json
{"ok":true,"generation":3,"keys":2,"streams":1,"changed":["plugins phi","tunnel allow +2/-0"],"restart_needed":[]}
```

`changed` names what differs from before, and `restart_needed` lists settings that need a restart, such as `tunnel.listen` or the `[log]` `file`, `format`, `stderr`, `file_max_bytes` and `file_keep`. Add `?dry_run=true` to preview (any other `dry_run` value than `true`, `1`, `false` or `0` answers `400` and applies nothing): the answer has `"dry_run":true`, says what would change, and nothing is applied or recorded in `/status`. A config that fails to parse or validate answers `422` with `{"ok":false,"error":"..."}` and leaves the running state alone. See [reload](troubleshooting.md#reload) for what applies live.

## GET /metrics

Served by the `metrics` plugin, so it exists only with the plugin built in and a `[plugins.metrics]` table (an empty table is enough); otherwise the path returns 404.

```toml
[plugins.metrics]
```

Prometheus text exposition, format version 0.0.4. `reiki_requests_total` counts each request once, by client-visible status and serving upstream (the fallback after a failover); a client that disconnects first counts as 499 (`client_error`). Gauges are sampled at scrape time. Counters survive a reload in memory; a changed `[plugins.metrics]` table keeps them and needs a restart, and series for renamed streams, upstreams and keys stay until restart.

| Metric | Type | Labels |
| --- | --- | --- |
| `reiki_requests_total` | counter | `stream`, `upstream`, `status` |
| `reiki_failovers_total` | counter | `stream`, `key`, `class` |
| `reiki_cooldowns_total` | counter | `key`, `class`, `scope` |
| `reiki_rate_limit_rejections_total` | counter | `stream` |
| `reiki_plugin_rejections_total` | counter | `plugin`, `stream` |
| `reiki_findings_total` | counter | `stream`, `plugin`, `kind`, `action` |
| `reiki_tokens_total` | counter | `stream`, `upstream`, `direction` |
| `reiki_keys` | gauge | `state` |

`reiki_findings_total` counts the identifiers a detection plugin (`phi`, `finance`, `controlled`) found in request bodies. `kind` is the identifier kind (`ssn`, `iban`); `action` is `blocked` when the request was rejected for it and `reported` when it went through. Label values are fixed by the plugins and configured streams, and the counter never holds a matched value. A plugin's `detail` label is ignored so series do not grow with reported names; use the request log for per-name counts.

`reiki_plugin_rejections_total` counts requests a plugin stopped: a rejection (the `phi`, `finance` and `controlled` plugins do when they block), or a skip or panic under `on_error = "reject"`, which answers 503. `plugin` is its name and `stream` the incoming stream, so series are bounded by built-in plugins times configured streams. A stream-rate-limited request counts under `reiki_rate_limit_rejections_total` instead.

`status` is `success`, `client_error` or `server_error`. `class` is `rate_limited`, `quota_exhausted`, `access_disabled`, `unauthorized` or `transient`. `reiki_cooldowns_total` also has `scope` (`key` or `model`): a model-scoped cooldown covers one route and model, deliberately not metric labels because they come from request bodies. The `reiki_keys` gauge always reports all three states, even at zero.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/metrics
```

## GET /usage

Served by the `metrics` plugin with the same bearer auth as `/status`, and under `/v1/usage`. Returns JSON per stream and upstream with `input` and `output` tokens and `requests` since start; `requests` counts only responses carrying `usage`. Tokens come from the provider's `usage` object: `prompt_tokens`/`completion_tokens` or `input_tokens`/`output_tokens`. For an event stream they come from whichever events carry `usage`, so Anthropic's `message_start` and `message_delta` are combined. JSON bodies over 1 MiB and responses without `usage` count nothing; a compressed response is not read, so its usage is not counted. The same totals are `reiki_tokens_total{stream,upstream,direction}` in `/metrics`.

```json
{"stream":{"upstream":{"input":10,"output":20,"requests":1}}}
```
