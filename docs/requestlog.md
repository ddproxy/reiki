# Request log plugin

The `requestlog` plugin records the requests that pass through Reiki as JSON lines. It writes them to a file, streams them live over a Unix socket, or both.

It is a bundled plugin, compiled in by the default `plugin-requestlog` feature and inactive until a `[plugins.requestlog]` table exists. Build with `--no-default-features` to leave it out.

## Configure

```toml
[plugins.requestlog]
file = "/var/lib/reiki/requests.jsonl"     # JSON lines, rotated by size
socket = "/run/reiki/requests.sock"        # live feed, one JSON line per record
body = "full"                              # "full", "summary" or "none"
headers = ["user-agent", "x-opencode-session"]
```

At least one of `file` and `socket` is required, both as absolute paths. At startup, a bad value, an unknown key or an unwritable path stops Reiki from starting.

| Key | Default | Meaning |
| --- | --- | --- |
| `file` | none | Append-only JSON lines file, created with mode 0600. An existing file is tightened to 0600. |
| `file_max_bytes` | 52428800 | Rotate when the next line would pass this size. `0` never rotates. |
| `file_keep` | 5 | Rotated files kept as `<file>.1` (newest) to `<file>.N`. `0` discards the old file at rotation. |
| `socket` | none | Unix socket created with mode 0600. It must sit in a directory that is not writable by everyone. A stale socket file is replaced; a socket another process is listening on, or any other kind of file at that path, is an error and is left alone. |
| `body` | `full` | `full` logs the request body, `summary` logs counts only, `none` logs its size only. |
| `max_body_bytes` | 65536 | Longest body text logged (1 to 16777216). Longer bodies are cut on a character boundary and marked `body_truncated`. |
| `headers` | none | Request headers to log, matched case-insensitively, each value cut at 256 bytes. Credential headers are refused (see below). |
| `response` | `status` | `status` writes a response record per request, `none` writes none. |
| `redact` | `true` | Mask credential-shaped text (see below). |
| `queue` | 1024 | Records buffered for the writer thread (16 to 65536). |

`enabled` and `on_error` are handled by Reiki itself, as for every plugin. `reiki check` validates this table without creating the file or the socket.

Stopping Reiki with a signal leaves the socket file behind; the next start replaces it.

## Records

Every record is one line with `"v":1`. A request produces two records that share an `id`.

```json
{"v":1,"event":"request","id":"d87b738bb70ccae6","ts":"2026-10-23T23:27:24.291Z","stream":"ct","upstream":"zen","method":"POST","headers":{"user-agent":"opencode/1.18.35"},"body_bytes":75,"body":{"model":"big-pickle","messages":[{"role":"user","content":"hello there"}]}}
{"v":1,"event":"response","id":"d87b738bb70ccae6","ts":"2026-10-23T23:27:24.540Z","status":200,"duration_ms":249,"bytes":11}
```

- `stream` is the Reiki stream (client) and `upstream` the provider name from the config.
- `body` is the parsed JSON when the body is JSON and fits `max_body_bytes`; otherwise it is a string. In `summary` mode the record has `body_summary` with `model`, `stream`, `max_tokens`, `temperature` and the number of `messages`, `tools` and `input_items`.
- The response record is written once the response has finished, or the client has gone away. `status` is the upstream status, and `null` when Reiki answered without an upstream response, for example when every key is cooling. `bytes` is the size of the response body as it passed through the plugin. Response bodies are never logged.
- If the writer cannot keep up, records are dropped rather than slowing requests, and a `{"event":"dropped","count":N}` record follows.

The log does not say which key served a request, and it does not include the request path; the plugin interface does not expose either.

## Watch it live

```bash
socat - UNIX-CONNECT:/run/reiki/requests.sock | jq -c '{event, id, stream, status}'
tail -f /var/lib/reiki/requests.jsonl | jq .
```

At most 32 subscribers connect at once. Each has its own bounded queue and is disconnected if it cannot keep up, so a slow reader never delays Reiki.

## What is kept out of the log

Request bodies are prompts, so treat the file and socket as sensitive. Both are readable by their owner only.

- **Credential headers are never logged.** `authorization`, `proxy-authorization`, `x-api-key`, `x-goog-api-key`, `cookie`, `set-cookie` and `x-auth-token` are refused in `headers`, so Reiki will not start with one listed. Upstream keys are never visible to a plugin.
- **Credential-shaped text is masked** with `[REDACTED]` in bodies and in the headers you do list: `sk-…`, `oc_sk_…`, `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`/`github_pat_` tokens, `glpat-`, `AKIA`/`ASIA` access keys, `AIza…`, `xox[bpas]-` tokens, JWT-shaped `eyJ…` strings, `Bearer <token>`, and PEM private key blocks.
- Masking is pattern based and applies to the text of JSON strings. It will not catch a secret in an unrecognised format, a password in prose, or personal data. Set `body = "summary"` or `"none"` where that matters.

## Where it sits in the chain

Reiki runs plugins in the order they are registered in the build, not in the order of the config. The log sees the request as it is at that point, after any plugin that runs before it.
