# Monitoring

Reiki can serve a small admin listener with `GET /status`, plus any paths a plugin adds such as `GET /metrics` from the `metrics` plugin. It is off unless you add an `[admin]` table, and it always listens on its own socket, never the client one.

## Admin API

Routes: `GET /status`, `POST /reload`, `GET /metrics` (when provided by a plugin), and all of the above under `/v1/...` with `GET /v1/openapi.json`. All admin routes require `Authorization: Bearer <admin token>`. Admin responses include `X-Reiki-Version`. The v1 API follows a stability promise: v1 only gains fields.

## Configure

```toml
[admin]
listen = "127.0.0.1:4098"                 # must differ from the client `listen`
token_sha256 = "<sha256 of the admin token>"
```

Both fields are required. The admin address is bound at startup, so adding, removing or moving `[admin]` needs a restart; a changed token hash is picked up on reload. Generate the hash with `printf '%s' "$ADMIN_TOKEN" | reiki hash-token`. `reiki check` prints the admin address alongside the client one.

## Authenticate

Send the admin token as a bearer credential: `Authorization: Bearer <token>`. A missing or wrong token gets a bare 401 with no body. The token is hashed and compared in constant time. Anything other than `/status` and a path a configured plugin serves returns 404. The listener serves plain HTTP, so keep it on loopback or put a TLS-terminating proxy in front of it.

## GET /status

With `[tunnel.intercept]` configured, the document also has `intercept.ca_not_after` (Unix seconds) and `intercept.ca_days_left`, so an expiring CA can be alerted on.

JSON with a `keys` array and a `streams` array.

Each key:

- `name`, `upstream`
- `state`: `available`, `cooling` or `out_of_rotation`
- `retry_after_seconds`: present only while `cooling`
- `consecutive_failures`: consecutive transient failures since the last success
- `last_status_code`: the upstream status of the key's latest attempt, or `null` before one

Each stream:

- `name`, `strategy` (`sticky` or `round_robin`), `chain`
- `current_key`: the key the stream would prefer next, or `null`
- `rate_limit_per_minute`: the stream's limit, or `null`

- `config`: `generation` (successful reloads since start), `loaded_at` and `last_reload`, which is `null` before any reload and otherwise has `ok`, `at` and, after a failure, `error`

The response carries no key values, token hashes or file paths.

## Check key health

`reiki keys` calls `GET /status` and prints one row per key: its name, upstream, state (`ok`, `cooling` or `disabled`), cooldown seconds remaining, last status code and the streams whose chain includes it. `reiki keys --json` prints a JSON document with a `keys` array instead.

```bash
reiki keys --token-file /etc/reiki/admin.token
reiki keys --url http://127.0.0.1:4098 --token-file /etc/reiki/admin.token --json
```

It takes `--url` and `--token-file` the same way as `reiki reload`, falling back to `[admin] listen` and `REIKI_ADMIN_TOKEN`. It exits 0 on success, 1 when the instance refuses the token or the answer, and 2 when it cannot reach the instance or resolve the admin address.

To check the key files from the config without a running instance, use `reiki keys --check`; see [troubleshooting](troubleshooting.md#key-health).

## POST /reload

Applies a changed config like `SIGHUP`, with the same bearer token as `/status`, and returns the result as JSON. A successful reload answers `200`:

```json
{"ok":true,"generation":3,"keys":2,"streams":1,"changed":["plugins phi","tunnel allow +2/-0"],"restart_needed":[]}
```

`changed` names what differs from before, and `restart_needed` lists settings that differ in the file but need a restart. Add `?dry_run=true` to preview (a `dry_run` value other than `true`, `1`, `false` or `0` answers `400` and applies nothing): the answer has `"dry_run":true` and says what a reload would change, and nothing is applied or recorded in `/status`. A config that fails to parse or validate answers `422` with `{"ok":false,"error":"..."}` and leaves the running state alone. See [reload](troubleshooting.md#reload) for what applies live.

## GET /metrics

Served by the `metrics` plugin, so it exists only when the binary includes the plugin and the config has a `[plugins.metrics]` table (an empty table is enough). Without that table the path returns 404.

```toml
[plugins.metrics]
```

Prometheus text exposition, format version 0.0.4. Gauges are sampled at scrape time; counters live in memory, survive a reload and reset when Reiki restarts.

| Metric | Type | Labels |
| --- | --- | --- |
| `reiki_requests_total` | counter | `stream`, `upstream`, `status` |
| `reiki_failovers_total` | counter | `stream`, `key`, `class` |
| `reiki_cooldowns_total` | counter | `key`, `class` |
| `reiki_rate_limit_rejections_total` | counter | `stream` |
| `reiki_findings_total` | counter | `stream`, `plugin`, `kind`, `action` |
| `reiki_keys` | gauge | `state` |

`reiki_findings_total` counts the identifiers a detection plugin (`phi`, `finance`, `gov`) found in request bodies. `kind` is the identifier kind, such as `ssn` or `iban`, and `action` is `blocked` when the request was rejected for it and `reported` when it went through. The label values are fixed by the plugins and the configured streams, and the counter never holds a matched value.

`status` is `success`, `client_error` or `server_error`. `class` is `rate_limited`, `quota_exhausted`, `access_disabled`, `unauthorized` or `transient`. The `reiki_keys` gauge always reports all three states, even at zero.

```bash
curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/metrics
```

## GET /usage

Served by the `metrics` plugin with the same bearer auth as `/status`, and under `/v1/usage`. Returns JSON per stream and upstream with `input` and `output` tokens and `requests` counted since start. Tokens come from the `usage` object a provider returns: `prompt_tokens` and `completion_tokens`, or `input_tokens` and `output_tokens`. For an event stream they come from whichever events carry `usage`, so Anthropic's `message_start` and `message_delta` are combined. JSON bodies over 1 MiB, and responses with no `usage`, count nothing. The same totals are `reiki_tokens_total{stream,upstream,direction}` in `/metrics`.

```json
{"stream":{"upstream":{"input":10,"output":20,"requests":1}}}
```
