# Request log plugin

The `requestlog` plugin records the requests that pass through Reiki as JSON lines. It writes them to a file, streams them live over a Unix socket, sends them to a syslog collector, or any mix of these.

Compiled in by the default `plugin-requestlog` feature; inactive until a `[plugins.requestlog]` table exists.

## Configure

```toml
[plugins.requestlog]
file = "/var/lib/reiki/requests.jsonl"     # JSON lines, rotated by size
socket = "/run/reiki/requests.sock"        # live feed, one JSON line per record
# syslog = "tcp://collector.internal:514"  # RFC 5424 messages, see Syslog output
body = "full"                              # "full", "summary" or "none"
headers = ["user-agent", "x-session-id"]
```

At least one of `file`, `socket` and `syslog` is required. File and socket paths are absolute. At startup, a bad value, an unknown key or an unwritable path stops Reiki from starting.

| Key | Default | Meaning |
| --- | --- | --- |
| `file` | none | Append-only JSON lines file, created with mode 0600. An existing file is tightened to 0600. |
| `file_max_bytes` | 52428800 | Rotate when the next line would pass this size. `0` never rotates. If a rename fails (a full or read-only directory, a stray file or directory at `<file>.N`), Reiki warns once, keeps writing to the current file, and retries after 1 second, doubling up to 60 seconds. No record is lost or written twice; the file stays over its limit until the rotation works. |
| `file_keep` | 5 | Rotated files kept as `<file>.1` (newest) to `<file>.N`. `0` discards the old file at rotation. |
| `socket` | none | Unix socket, mode 0600, path at most 100 bytes. The socket is created first and restricted a moment later, so put it in a directory that is not world-writable, ideally private (0700). A stale socket file is replaced; a live listener or any other file at that path is an error and is left alone. |
| `syslog` | none | Syslog collector: `udp://host:port`, `tcp://host:port` or `unix:///dev/log`. Each record is sent as an RFC 5424 message. See [Syslog output](#syslog-output). |
| `syslog_facility` | `local0` | RFC 5424 facility name, from `kern` to `local7`. Needs `syslog`. |
| `syslog_app_name` | `reiki` | APP-NAME: 1 to 48 printable ASCII characters, no spaces, not `-`. Needs `syslog`. |
| `body` | `full` | `full` logs the request body, `summary` logs counts only, `none` logs its size only. |
| `max_body_bytes` | 65536 | Longest body text logged (1 to 16777216). Longer bodies are cut on a character boundary and marked `body_truncated`. |
| `max_capture_bytes` | 1048576 | Most body bytes copied per request onto the writer queue (1 to 67108864), so the queue's worst case is `queue` times this. A request body over it is cut, logged as masked text and marked `body_truncated`; `body_bytes` still has the full size. It also caps the response body and error bodies kept for `response = "body"`, together with `max_body_bytes`. |
| `headers` | none | Request headers to log, matched case-insensitively, each value cut at 256 bytes. Credential headers are refused (see below). |
| `response` | `status` | `status` writes a response record per request with its status, size, serving key and attempts. `body` adds the response body, cut at `max_body_bytes`. `none` writes no response record. |
| `exclude` | none | Top-level keys or JSON pointers removed from logged JSON bodies, for example `["tools", "/messages/0"]`. The record lists what was dropped in `body_excluded`. |
| `summarize` | none | Paths replaced by a short description: an array of named items (such as tools) becomes the list of names, any other array becomes `"[N items]"`, an object becomes its key names. |
| `max_string_bytes` | `0` | Longest string kept in a logged JSON body; `0` keeps them whole. Longer ones are cut and end in `…[+N bytes]`. |
| `format` | `json` | `json` writes Reiki's own records. `ocsf` writes [OCSF](https://schema.ocsf.io/) 1.3 events for a SIEM: see [OCSF output](#ocsf-output). |
| `sse` | `events` | How an event-stream response is logged under `response = "body"`: `events` (an array of the parsed `data:` events), `assembled` (the streamed text, finish reason and usage) or `raw` (the stream's text). |
| `redact` | `true` | Mask credential-shaped text (see below). |
| `queue` | 1024 | Records buffered for the writer thread (16 to 65536). |

`enabled` and `on_error` are handled by Reiki itself, as for every plugin. `reiki check` validates this table without creating the file or the socket.

Stopping Reiki with a signal leaves the socket file behind; the next start replaces it.

## Records

Every record is one line with `"v":1`. A request produces two records that share an `id`.

```json
{"v":1,"event":"request","id":"d87b738bb70ccae6","ts":"2026-10-23T23:27:24.291Z","stream":"ct","upstream":"example","method":"POST","headers":{"user-agent":"client/1.0"},"body_bytes":75,"body":{"model":"small","messages":[{"role":"user","content":"hello there"}]}}
{"v":1,"event":"response","id":"d87b738bb70ccae6","ts":"2025-01-01T12:00:00.000Z","status":200,"duration_ms":249,"bytes":11,"key":"example-primary","attempts":[{"key":"example-primary","status":200}]}
```

- `stream` is the Reiki stream (client) and `upstream` the provider name from the config.
- `body` is parsed JSON when the body is JSON and fits `max_body_bytes` after `exclude`, `summarize` and `max_string_bytes` have trimmed it; otherwise it is a string. `body_bytes` is always the original size. Trim with those options instead of raising the cap, so large bodies stay parsed JSON.
- In `summary` mode the record has `body_summary` with `model`, `stream`, `max_tokens`, `temperature` and the counts of `messages`, `tools` and `input_items`.
- The response record is written once the response has finished, or the client has gone away. `status` is the upstream status, and `null` when Reiki answered without an upstream response, for example when every key is cooling. `bytes` is the size of the response body as it passed through the plugin. Response bodies are never logged.
- If the writer cannot keep up, records are dropped rather than slowing requests, and a `{"event":"dropped","count":N}` record follows. A record only the syslog collector misses is reported to syslog alone, see [Syslog output](#syslog-output).

The request record carries the request `path` without its query string. The response record carries `key`, the name in the config of the key that produced the response (`null` when Reiki answered itself), and `attempts`, every key tried in order with the upstream status it returned (`null` for a connection error or timeout). Only key names are logged, never a key's value.

With `response = "body"`, each failed entry in `attempts` also carries the upstream's error `body`, and the response record carries `body`, the leading part of the response body. It is parsed JSON when the whole body is JSON and fits; an event stream is parsed according to `sse` and marked `"body_format":"sse"`; anything else is text. It is masked like the request body, and `body_truncated` is set when it was cut at `max_body_bytes`.

Responses Reiki generates itself, such as the 503 when every key is unavailable, bypass the plugins and have no body here.

## Watch it live

```bash
socat - UNIX-CONNECT:/run/reiki/requests.sock | jq -c '{event, id, stream, status}'
tail -f /var/lib/reiki/requests.jsonl | jq .
```

At most 32 subscribers connect at once. Each has its own bounded queue and is disconnected if it cannot keep up, so a slow reader never delays Reiki.

## What is kept out of the log

Request bodies are prompts, so treat the file and socket as sensitive. Both are readable by their owner only.

- **Credential headers are never logged.** `authorization`, `proxy-authorization`, `x-api-key`, `x-goog-api-key`, `cookie`, `set-cookie` and `x-auth-token` are refused in `headers`, so Reiki will not start with one listed. Upstream keys are never visible to a plugin.
- **Credential-shaped text is masked** with `[REDACTED]` in bodies and listed headers: `sk-…`, `sk_live_`/`sk_test_`/`rk_live_`/`whsec_` (Stripe), `oc_sk_…`, `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`/`github_pat_` tokens, `glpat-`/`glptt-`, `AKIA`/`ASIA` access keys, `AIza…` and `ya29.` Google keys and tokens, `xox[bpas]-` tokens, `hf_`, `gsk_`, `xai-`, `pplx-`, `nvapi-`, `csk-`, `jina_`, `tvly-`, `r8_`, `npm_`, `pypi-`, `shpat_`, `SG.`, `dop_v1_`, `lin_api_`, `sbp_`, `dckr_pat_`, `hvs.` and `ATATT3` tokens, JWT-shaped `eyJ…` strings, `Bearer <token>` and PEM private key blocks.
- **Encoded credentials are masked** when the whole value is one: a base64 (standard or URL-safe) value that decodes to one of the shapes above or to a PEM private key, and a percent-encoded run, such as `sk%2D…` or `Bearer%20…`, that decodes to one. A key inside a larger base64 value is not found.
- **Fields named like credentials** have their string value masked whatever it holds: a field named `password`, `passwd`, `secret`, `token`, `api_key`, `auth`, `authorization`, `cookie` or `credentials`, or ending in `_password`, `_secret`, `_token`, `_apikey`, `_secret_key`, `_private_key` or `_access_key`, matched without regard to case, dashes and underscores. Only a string is replaced, so a schema such as `"password": {"type": "string"}` and a number such as `max_tokens` are kept.
- **A credential split across stream events** is masked: for an event-stream response the text deltas (`choices[0].delta.content`, `reasoning_content`, `delta.text`, `delta.partial_json`) are read as one text per field, and a credential that spans events is masked in every piece. `sse = "raw"` keeps the stream's text line by line and does not join events.
- **Masking comes before cutting.** When a capture stops at `max_capture_bytes`, `max_body_bytes` or an error body's limit, a credential cut off at the end is masked too, even if it is shorter than its usual minimum length.
- Headers that carry credentials (`authorization`, `proxy-authorization`, `cookie`, `set-cookie`, `x-api-key`, `x-goog-api-key`) are masked whole.
- The logged path has credential query parameters (`key`, `token`, `api_key`, `access_token` and similar) and credential-shaped segments masked. JSON object keys are masked as well as values.
- Masking is pattern based and applies to the text of JSON strings. It will not catch a secret in an unrecognised format, a password in prose, or personal data. Set `body = "summary"` or `"none"` where that matters.

## Where it sits in the chain

Reiki runs plugins in the order they are registered in the build, not in the order of the config. The log sees the request as it is at that point, after any plugin that runs before it.

## Findings from detection plugins

When `phi`, `finance` or `controlled` finds identifiers in a request, the log gets a `finding` record with the same `id` as the request: `{"event":"finding","id":"...","plugin":"phi","kind":"ssn","count":2,"action":"reported"}`. `action` is `blocked` or `reported`. The record names a kind and a count and never the matched text.

A request a plugin blocks is rejected before it reaches the request log, so it has a `finding` record and no `request` or `response` record.

A finding can also carry a `detail` label, such as a tool name, when the plugin reports one. It is a short sanitised string (see [Report findings with a detail](plugins.md#report-findings-with-a-detail)), and the record has no `detail` field when the plugin sent none.

The request log does not mask detected identifiers in the body it records. With the default `body = "full"`, a request that `phi` only reports is written to the log with its identifiers in it. Where a detection plugin runs in `report` mode, set `body = "summary"` or `body = "none"`.

## OCSF output

`format = "ocsf"` writes each record as an OCSF 1.3 event, one JSON object per line, to the file and the socket:

| Record | OCSF class | Notes |
| --- | --- | --- |
| request | HTTP Activity (4002) | `activity_id` from the method, `http_request` with the redacted path and headers, `src_endpoint` the stream, `dst_endpoint` the upstream |
| response | HTTP Activity (4002) | `http_response.code`, `duration` in milliseconds, `status_id` 1 or 2 |
| finding | Detection Finding (2004) | `finding_info.types` is the kind, `count`, and `disposition` `Blocked` or `Logged`. With a `detail`, `finding_info.title` ends in `(kind:detail)` |

Every event carries `metadata.correlation_uid`, the request id, so a SIEM can join a request, its response and its findings. Redaction, `headers` and `body` apply as in the JSON format.

## Syslog output

`syslog` sends each record to a syslog collector as an RFC 5424 message. The message body is the same JSON line or OCSF event that the file gets, so `format` applies here too.

```toml
[plugins.requestlog]
syslog = "tcp://collector.internal:514"
syslog_facility = "local0"
syslog_app_name = "reiki"
```

`syslog_facility` accepts the RFC 5424 names: `kern`, `user`, `mail`, `daemon`, `auth`, `syslog`, `lpr`, `news`, `uucp`, `cron`, `authpriv`, `ftp`, `ntp`, `audit`, `alert`, `clock`, and `local0` to `local7`.

### Transports

- `udp://host:port` sends one message per datagram, with no framing. A message larger than the datagram limit is dropped and counted.
- `tcp://host:port` frames each message by its octet count, `LEN SP MSG`, as RFC 6587 describes. A write that waits longer than one second counts as a failure.
- `unix:///dev/log` sends one message per datagram to a Unix datagram socket. Stream sockets are not supported. The path must be absolute and at most 100 bytes.

IPv6 hosts go in brackets, such as `tcp://[::1]:514`.

### Field mapping

| Field | Value |
| --- | --- |
| PRI | Facility times 8, plus severity. Severity is 6 (info) for requests and responses, and 4 (warning) for findings, responses with a status of 500 or more, and `dropped` records. |
| VERSION | `1` |
| TIMESTAMP | The record's time, RFC 3339 in UTC with milliseconds. |
| HOSTNAME | The host name from `/proc/sys/kernel/hostname`, or from `/etc/hostname`. `-` when neither gives a usable name. |
| APP-NAME | `syslog_app_name` |
| PROCID | Reiki's process id. |
| MSGID | `request`, `response`, `finding` or `dropped`. |
| STRUCTURED-DATA | `-` |
| MSG | The record, as a JSON line or an OCSF event, as `format` selects. |

A response that Reiki generates itself has no upstream status, so it is logged at info. An example is the 503 returned when every key is cooling.

Example message:

```
<134>1 2026-10-23T23:27:24.291Z gw-1 reiki 4242 request - {"v":1,"event":"request","id":"d87b738bb70ccae6",...}
```

### Delivery

The writer thread connects on the first record, not at startup, so Reiki starts while the collector is down. A failed connect or write drops the record. The next attempt waits 250 milliseconds. Each further failure doubles the wait, up to 30 seconds. A successful connect resets it.

Records that arrive during the wait are dropped without a connect attempt. The file and the socket still have them. Once a send succeeds again, Reiki sends one `dropped` record with the count to the collector only.

Requests never wait for the collector.

### Example collector

An rsyslog collector that receives the TCP feed and writes Reiki's records to their own file:

```
module(load="imtcp")
input(type="imtcp" port="514")

if $programname == 'reiki' then {
    action(type="omfile" file="/var/log/reiki/requests.log")
    stop
}
```

Open the collector's port only to the hosts that send to it. The records hold prompts, so keep the collector's files readable by its operators only.

