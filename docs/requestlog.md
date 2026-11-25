# Request log plugin

`requestlog` records requests that pass through Reiki as JSON lines. It can write a file, stream over a Unix socket, send to a syslog collector, or run a separate summary-only export for a log collector. It is compiled in by the default `plugin-requestlog` feature and is inactive until a `[plugins.requestlog]` table exists.

## Configure

```toml
[plugins.requestlog]
file = "/var/lib/reiki/requests.jsonl"     # JSON lines, rotated by size
socket = "/run/reiki/requests.sock"        # live feed, one JSON line per record
# syslog = "tcp://collector.example.com:514"
# capture_bodies = false
body = "summary"                            # "summary", "full" or "none"
headers = ["user-agent", "x-session-id"]
```

At least one of `file`, `socket`, `syslog` and `[plugins.requestlog.export]` is required. Paths are absolute; a bad value, unknown key or unwritable path stops startup. `reiki check` validates the table without creating files or sockets.

| Key | Default | Meaning |
| --- | --- | --- |
| `file` | none | Append-only JSON lines, mode `0600`. A symlink, directory or device at the path is refused; an existing regular file is tightened to `0600`. |
| `file_max_bytes` | 52428800 | Rotate when the next line would pass this size; `0` never rotates. A failed rename warns once, keeps writing and retries with backoff up to 60 seconds; no record is lost or duplicated. |
| `file_keep` | 5 | Rotated files kept as `<file>.1` (newest) to `<file>.N`; `0` discards at rotation. |
| `socket` | none | Unix socket, mode `0600`, path at most 100 bytes. Created before it is restricted, so use a private directory. A stale socket file is replaced; a live listener or other file is an error. |
| `syslog` | none | `udp://host:port`, `tcp://host:port` or `unix:///dev/log`; see [syslog output](#syslog-output). |
| `syslog_facility` | `local0` | RFC 5424 facility name, `kern` to `local7`. Needs `syslog`. |
| `syslog_app_name` | `reiki` | APP-NAME: 1 to 48 printable ASCII characters, no spaces, not `-`. Needs `syslog`. |
| `capture_bodies` | false | Permit body text. Enabling warns at startup and applies to file, socket and syslog together. |
| `body` | `summary` | `summary` records counts and scalar settings, never prompt or tool text; `none` records the body size only; `full` needs `capture_bodies = true`. |
| `max_body_bytes` | 65536 | Longest logged body text when capturing (1 to 16777216); longer bodies are cut on a character boundary and marked `body_truncated`. |
| `max_capture_bytes` | 1048576 | Most body bytes copied onto the writer queue per request; also caps response and error bodies. In summary mode it bounds bytes inspected. |
| `headers` | none | Request headers to log, case-insensitive, each value cut at 256 bytes. Credential headers are refused. |
| `response` | `status` | `status` writes a response record per request; `body` adds response and failed-attempt bodies and needs `capture_bodies = true`; `none` writes none. |
| `exclude` | none | Top-level keys or JSON pointers removed from logged JSON bodies, listed in `body_excluded`. |
| `summarize` | none | Paths replaced by short descriptions: named item lists become names, other arrays `"[N items]"`, objects their key names. |
| `max_string_bytes` | 0 | Longest string kept in a logged JSON body; `0` keeps them whole. |
| `format` | `json` | `json` writes Reiki records; `ocsf` writes OCSF 1.3 events; see [OCSF output](#ocsf-output). |
| `sse` | `events` | For `response = "body"`: `events` (parsed `data:` events), `assembled` (text, finish reason and usage) or `raw` (stream text). |
| `redact` | true | Mask credential-shaped text in bodies and listed headers. |
| `queue` | 1024 | Records buffered for the writer thread (16 to 65536). |
| `queue_max_bytes` | 67108864 | Most bytes of queued records; a record over the limit is dropped, counted and warned once. Requests are never delayed. |

Stopping Reiki with a signal leaves the socket file behind; the next start replaces it. At most 32 subscribers can watch the socket, each with a bounded queue; a slow or gone subscriber is disconnected on broadcast.

## Summary export

`[plugins.requestlog.export]` runs a second independent writer for log collectors. It is off when absent. The main sinks keep their own format and body settings; the export always writes the fixed `request_summary` shape below, with no body text, header values, query strings or credential values. An export-only config is valid without any main sink.

```toml
[plugins.requestlog.export]
format = "jsonl"                                # default
file = "/var/lib/reiki/export.jsonl"
max_bytes = 10485760                            # default
max_files = 3                                   # default
queue = 256                                     # default
```

```toml
[plugins.requestlog.export]
format = "syslog"
syslog = "udp://collector.example.com:514"      # or unix:///absolute/path
syslog_facility = "local0"                      # default
syslog_app_name = "reiki-export"                # default
queue = 256
```

`tcp://` is refused for the export because its writer must never block a request. `file` and `syslog` are mutually exclusive and each format needs its own target; unknown keys are a config error.

| Key | Default | Meaning |
| --- | --- | --- |
| `format` | `jsonl` | `jsonl` writes one JSON object per line; `syslog` writes an RFC 5424 message. |
| `file` | none | Absolute rotating export path, mode `0600`; required by `jsonl`, refused by `syslog`. |
| `max_bytes` | 10485760 | Rotate when the next line would pass this size. |
| `max_files` | 3 | Rotated files kept as `<file>.1` (newest) to `<file>.N`; `0` discards at rotation. |
| `syslog` | none | `udp://host:port` or `unix:///absolute/path`; required by `syslog`, refused by `jsonl`. |
| `syslog_facility` | `local0` | RFC 5424 facility name. |
| `syslog_app_name` | `reiki-export` | APP-NAME: 1 to 48 printable ASCII characters, no spaces, not `-`. |
| `queue` | 256 | Summaries buffered for the writer (1 to 65536); a full queue drops the newest, never delaying the request. |

Every summary has exactly these fields; additions are announced in the [changelog](../CHANGELOG.md).

| Field | Type | Meaning |
| --- | --- | --- |
| `v` | integer | Shape version, currently `1`. |
| `event` | string | Always `request_summary`. |
| `ts` | string | UTC finish time, RFC 3339 with milliseconds. |
| `id` | string | Request id shared with the service log; 32 hex digits. |
| `method` | string | HTTP method. |
| `route` | string | Redacted path without query string or fragment. |
| `upstream` | string | Configured provider name, not its base URL. |
| `status` | integer or null | Client-visible status, or `null` when Reiki answered without an upstream. |
| `duration_ms` | integer | Total request duration. |
| `head_ms` | integer | Time to the response head, `0` when none arrived. |
| `input_tokens` | integer | Provider-reported input tokens, `0` when absent or over the parse cap. |
| `output_tokens` | integer | Provider-reported output tokens, `0` when absent or over the parse cap. |
| `key` | string or null | Config name of the serving key, never its value; `null` when Reiki answered itself. |
| `failure_class` | string or null | `rate_limited`, `quota_exhausted`, `unauthorized`, `transient`, `request_error`, `transport_error`, or `null` for success. |

Token counts come from JSON or server-sent `usage` objects, accepting both `prompt_tokens`/`completion_tokens` and `input_tokens`/`output_tokens`. A record that cannot be queued, or a syslog datagram that cannot be sent, increments `reiki_requestlog_export_dropped_total`, exposed at `GET /v1/requestlog/metrics`, as `requestlog.export_dropped_total` in `/v1/status` and as `requestlog_export_dropped_total` in `/v1/health`. See [monitoring](monitoring.md#get-v1requestlogmetrics). Ship `jsonl` with a tail input or `syslog` with an rsyslog listener filtered by APP-NAME; open the collector port only to senders you control.

## Stability of records

Every JSON record carries `"v":1`; OCSF output uses `metadata.version` for the external schema version and syslog `VERSION 1` for the RFC 5424 protocol. Request records contain `v`, `event`, `id`, `ts`, `stream`, `upstream`, `method`, `body_bytes` and optional `headers`, `body`, `body_summary`, `body_excluded` and `body_truncated`. Response records contain `v`, `event`, `id`, `ts`, `status`, `duration_ms`, `bytes`, `key` and optional `attempts`. Finding records contain `v`, `event`, `id`, `stream`, `plugin`, `kind`, `count`, `action` and optional `detail`. Dropped records contain `v`, `event`, `ts` and `count`.

`status` and `key` are `null` when Reiki generated the response. Join a request to its response and findings by `id`; ignore unknown fields and tolerate missing ones. Field changes are announced in the [changelog](../CHANGELOG.md).

## Records

A request produces two records with the same `id`:

```json
{"v":1,"event":"request","id":"d87b738bb70ccae6","ts":"2026-10-23T23:27:24.291Z","stream":"client-a","upstream":"example","method":"POST","headers":{"user-agent":"client/1.0"},"body_bytes":75,"body_summary":{"model":"small","messages":1}}
{"v":1,"event":"response","id":"d87b738bb70ccae6","ts":"2026-10-23T23:27:24.291Z","status":200,"duration_ms":249,"bytes":11,"key":"example-primary","attempts":[{"key":"example-primary","status":200}]}
```

- `body_bytes` is always the original size. Default `summary` mode yields `body_summary` with `model`, `stream`, `max_tokens`, `temperature` and counts of `messages`, `tools` and `input_items`, never message contents or tool arguments. `body = "none"` records size only.
- With `capture_bodies = true` and `body = "full"`, `body` is parsed JSON when the trimmed body is JSON and fits `max_body_bytes`, otherwise a string. Trim with `exclude`, `summarize` and `max_string_bytes` rather than raising the cap.
- The response record is written when the response finishes or the client goes away. `status` is the upstream status, `null` when Reiki answered itself. `bytes` is the body size as it passed the plugin; `duration_ms` is elapsed time. Responses Reiki generates itself bypass plugins and have no body here.
- With `response = "body"`, failed `attempts` carry the upstream error `body` and the response record carries its leading `body`, parsed as JSON, SSE according to `sse`, or text. It is masked and marked `body_truncated` when cut.
- If the writer cannot keep up, records are dropped and a `{"v":1,"event":"dropped","ts":"...","count":N}` record follows. The first drop also logs one warning.

## Watch it live

```sh
socat - UNIX-CONNECT:/run/reiki/requests.sock | jq -c '{event, id, stream, status}'
tail -f /var/lib/reiki/requests.jsonl | jq .
```

## What is kept out of the log

Request bodies are prompts, so they are not recorded by default. If you opt in with `capture_bodies = true`, treat the file, socket and syslog collector as sensitive.

- No bodies by default. `capture_bodies = false` keeps prompt text, response text, tool arguments and failed-attempt error bodies out of records; `body = "summary"` and `response = "status"` need no opt-in.
- Every captured body is masked and capped before being written to any sink. Masking runs before cuts at `max_capture_bytes`, `max_body_bytes` and error-body limits, so a credential cut at the end is masked too.
- Credential headers are never logged. A `headers` entry whose name contains `auth`, `token`, `key`, `secret`, `cookie` or `credential` is refused at startup, covering `authorization`, `proxy-authorization`, `x-api-key`, `x-goog-api-key`, `cookie`, `set-cookie` and `x-auth-token`.
- Credential-shaped text in bodies and listed headers is replaced with `[REDACTED]`: common API key prefixes, bearer tokens, JWT-shaped strings, PEM private key blocks and similar provider tokens.
- Encoded credentials are masked when a base64 or percent-encoded value decodes to one, including a base64 `user:credential` or JSON token object.
- JSON fields named like credentials have their string values masked regardless of content: `password`, `passwd`, `secret`, `token`, `api_key`, `auth`, `authorization`, `cookie`, `credentials` and suffixes such as `_password`, `_secret`, `_token`, `_apikey`, `_secret_key`, `_private_key`, `_access_key`, matched without regard to case, dashes or underscores. Only strings are replaced; schemas and numeric fields are kept.
- For event streams, text deltas are joined per field so a credential split across events is masked in every piece. `sse = "raw"` keeps lines separate.
- Logged paths mask credential query parameters (`key`, `token`, `api_key`, `access_token`, `auth_token`, `api_secret`, `password`, `signature` and similar) and credential-shaped segments; names are percent-decoded and matched ignoring case, dashes and underscores. The same rule masks form-body values in requests and responses. JSON object keys are masked as well as values.
- Masking is pattern based. It will not catch a secret in an unrecognised format, a password in prose or personal data. Keep `capture_bodies = false`, or use `body = "none"` where summaries are too much.

## Where it sits in the chain

Plugins run in build registration order, not config order. The log sees the request as it is after earlier plugins; in the default build `placeholders` runs before `requestlog`, and response hooks run in reverse, so the log sees masked responses before restoration. See [plugins](plugins.md#choose-and-order-plugins-at-build-time).

## Findings from detection plugins

When `phi`, `finance` or `controlled` finds an identifier, the log gets a `finding` with the request's `id`:

```json
{"v":1,"event":"finding","id":"d87b738bb70ccae6","stream":"client-a","plugin":"phi","kind":"ssn","count":2,"action":"reported"}
```

`action` is `blocked` or `reported`; matched text is never logged. A request a plugin blocks is rejected before it reaches the request log, so it has a `finding` and no `request` or `response` record. A finding can carry a short sanitised `detail` label; see [report findings with a detail](plugins.md#report-findings-with-a-detail). The request log does not mask detected identifiers: with `capture_bodies = true` and `body = "full"`, a request a detector only reports is written with its identifiers. Leave body capture off, or set `body = "none"`, where a detector runs in `report` mode.

## OCSF output

`format = "ocsf"` writes one OCSF 1.3 event per line to the file and socket. Requests and responses are HTTP Activity (4002) events with `activity_id` from the method, the redacted path and headers, stream and upstream endpoints, `http_response.code` and `duration`. Findings are Detection Finding (2004) events with `finding_info.types` as the kind, `count` and `disposition` `Blocked` or `Logged`; a `detail` appends `(kind:detail)` to `finding_info.title`. `metadata.correlation_uid` is the request id, joining a request, response and findings.

## Syslog output

`syslog` sends each record as an RFC 5424 message; the body is the same JSON line or OCSF event the file gets.

```toml
[plugins.requestlog]
syslog = "tcp://collector.example.com:514"
syslog_facility = "local0"
syslog_app_name = "reiki"
```

- `udp://host:port` sends one message per datagram; an oversized message is dropped and counted.
- `tcp://host:port` frames messages by octet count (`LEN SP MSG`); a write waiting over one second fails.
- `unix:///dev/log` sends one datagram to a Unix datagram socket; the path must be absolute and at most 100 bytes. IPv6 hosts go in brackets, such as `tcp://[::1]:514`.

PRI is facility times eight plus severity: 6 (info) for requests and responses, 4 (warning) for findings, 500-and-over responses and `dropped` records. TIMESTAMP is RFC 3339 UTC, HOSTNAME comes from the host, PROCID is the Reiki process id, MSGID is the record type and STRUCTURED-DATA is `-`. A response Reiki generates itself has no upstream status and is logged at info.

The writer connects on the first record, so Reiki starts while the collector is down. A failed connect or write drops the record; the next attempt waits 250 ms and doubles up to 30 seconds, resetting on success. Once sending recovers, Reiki sends one `dropped` record with the count to the collector only. Requests never wait for the collector; with `capture_bodies = true`, keep collector files readable by operators only.
