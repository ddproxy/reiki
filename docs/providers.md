# Providers

Each provider is an `[upstream.<name>]` table in `reiki.toml`. A client reaches it through Reiki at `http://127.0.0.1:4097/<name>`. See [configuration](configuration.md) for every field.

## How a provider maps

- Set `base_url` to the root the provider's SDK uses by default.
- Set the client's base URL to `http://127.0.0.1:4097/<name>`, so the SDK appends its usual paths and Reiki forwards them to `base_url`.
- Set `auth_header` and `auth_format` to the provider's expectation; defaults are `authorization` and `Bearer {key}`. A bad pair is refused when the config loads.
- Clients send a stream token in `Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or `?key=`, whichever the SDK uses. Headers win; the query parameter is last. See [keys](keys.md).

A stream can chain keys from several providers; a request uses only keys from the provider in its path.

## Provider status and test coverage

No test calls a live provider. Integration tests point an upstream at a local mock listener, socket or command. "Covered" means a test exists in this repository, not that the provider's current API has been checked.

| Provider or profile | Config mapping | Covered by tests | Not verified here |
| --- | --- | --- | --- |
| OpenAI | `base_url = "https://api.openai.com/v1"`; default Bearer mapping | Bearer and `x-api-key` extraction, path and credential swap, 401 disable and 429 rules, 402 request-size pass-through | Real endpoint, SDK and exhausted key |
| OpenCode Zen | `base_url = "https://opencode.ai/zen/v1"`; default Bearer, with `quota_phrases = ["free-models-per-day"]` when that body must count as quota | Aliases and dropped params, free-usage 429, configured phrase classification, `confirm_quota`, scoped 402/429 | Real account, free-tier headers, paid-model 402, `/responses` probe, `scope_escalate_after` |
| Anthropic | `x-api-key`, `{key}` | `/anthropic/v1/messages` path with `anthropic-version`, placeholder and SSE handling; 400 credit-balance classification | Real endpoint; the path test does not send the documented quota body |
| Gemini | `x-goog-api-key`, `{key}` | `x-goog-api-key` extraction, `?key=` accepted then stripped, 400 `API_KEY_INVALID` classification | End-to-end profile and real response |
| Other OpenAI-compatible | OpenAI mapping with the service's base URL | Generic 402 and configured 429 classifier units; failover and limits tests | Base-URL integration tests; Groq- and Together-specific tests |
| Passthrough | `passthrough = true`; no Reiki token or key | Client credentials and `key=` query forwarded, no key used, no failover | Real provider TLS handshake and HTTP version |
| Forwarders | `[upstream.X.forward]` socket or command | Socket and command round trips, failover, uid check, streaming, timeouts, concurrency | A real listener or program outside tests |
| Fallback and transport | `[upstream.X.fallback]`, limits and transport flags | Exhaustion fallback, budgets, compression and header settings; one hop is enforced in `src/proxy.rs` | One-hop behaviour has no dedicated test; HTTP/2 over TLS is not exercised |

## Rewriting the request body

- `model_aliases = { "gpt-4o" = "opencode/gpt-4o" }` replaces the top-level `model` string when it matches a key.
- `drop_params = ["stream_options"]` removes the listed top-level keys.

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
model_aliases = { "gpt-4o" = "opencode/gpt-4o" }
drop_params = ["stream_options"]
```

Rewriting runs once before a key is picked, so failover replays the same bytes. A non-object body, a missing or non-string `model`, an unknown model and a non-matching `drop_params` pass through byte-for-byte. Only top-level keys are dropped. Keys and `model` are compared after JSON escapes are decoded. A body with more than 4096 top-level members is refused with `400 too_many_members`.

## Transport options

By default Reiki uses HTTP/1.1, asks for uncompressed responses and sends lowercase header names.

- `http2 = true` negotiates HTTP/2 when the upstream offers it over TLS.
- `forward_accept_encoding = true` sends the client's `Accept-Encoding` upstream instead of `identity`; a client that sent none still gets `identity`. Reiki decodes gzip, deflate, brotli and zstd for failover and logging; the client gets plain bytes without `Content-Encoding`. A forwarder relays the coding as-is, and an undecodable coding with a key in play fails with 502.
- `max_decoded_bytes` caps the decoded body, compressed responses included. Default 64 MiB. A decoded non-SSE body over the cap becomes a 502; an SSE body ends with an error. Compressed bytes are capped at max(2x the cap, 1 MiB).
- `response_idle_timeout_secs` drops a body with no chunk for that many seconds (default 300): buffered bodies answer 504, streamed bodies end with an error. `response_timeout_secs` bounds the whole body from its head; unset means 300 for a non-SSE body and no limit for an event stream.
- `title_case_headers = true` sends Title-Case names on HTTP/1.1; HTTP/2 always lowercases. `send_keep_alive = true` adds `Connection: keep-alive`. Both are refused with `forward`.

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
http2 = true
forward_accept_encoding = true
```

The credential header goes first, ahead of the client's headers. These settings apply to reverse-proxy routes; an intercepted tunnel relays traffic as the client sent it. They change how Reiki connects, not which requests an upstream accepts. See [tunnel](tunnel.md).

## Outbound limits

`[upstream.NAME.limits]` gives each key of that upstream a budget: requests and tokens per minute, concurrency and daily totals. Without the table keys are never shaped. Reiki takes a slot before sending, waits within `max_wait_secs` and moves to the next key when that key cannot serve. When no key is usable the client gets `429 reiki_shaped` with `Retry-After` and `x-reiki-origin: local`.

A key's bucket learns from its own responses: `Retry-After` and `remaining: 0` pause or close it, quota failures close it until the cooldown ends, and quiet successes raise a learned cap. Learned caps, pauses, closed windows and daily counters persist in `<config>.state`; the minute log does not. See [shaping](shaping.md), [failover](failover.md) and [configuration](configuration.md) for presets and fields.

## Forwarding

A forward upstream hands each request to a listener or a program instead of sending it itself.

```toml
[upstream.example.forward]
socket = "/run/forwarder.sock"
expect_uid = 1000
```

```toml
[upstream.example.forward]
command = "/usr/local/bin/forward-request"
args = ["--config", "/etc/forward.toml"]
pass_env = ["FORWARD_TARGET"]
```

Set exactly one of `socket` or `command`. Add `streaming = true` to relay the answer as it is produced.

### What the forwarder receives

Reiki is the HTTP client and speaks plain HTTP/1.1. The request line uses the upstream URL's path and query, and `Host` is the upstream host:

```http
POST /v1/chat?x=1 HTTP/1.1
Host: example.com
X-Forwarded-Proto: https
Connection: close
Content-Length: 2
Authorization: Bearer <key>
Content-Type: application/json

{}
```

Other client headers pass through as for a normal upstream. The key is sent only in the upstream's credential header, with no second copy and no Reiki-specific header; `passthrough` forwards the client's credential unchanged. Client-sent `Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto` and `X-Real-IP` are replaced or removed, as are hop-by-hop headers and headers named by the client's `Connection`.

### What Reiki does with the answer

Without `streaming`, the body is buffered whole: up to 8 MiB, 128 headers and 64 KiB per header value; chunked responses are joined. Reiki sets `content-length` to the buffered length (HEAD keeps the forwarder's length; 204 and 304 carry none), relays `content-encoding` without decoding it and removes hop-by-hop headers. Nothing reaches the client until the forwarder finishes.

With `streaming = true`:

- The response returns as soon as its head is in; the body is unbuffered but bounded by `max_stream_bytes` (default 64 MiB) and `stream_timeout_secs` (default 1800). Passing either limit ends the stream.
- The 120-second request timeout bounds only the wait for the head.
- A reported `content-length` is kept; otherwise the body is chunked. `transfer-encoding` and other hop-by-hop headers are removed. An undecodable `content-encoding` with a key in play fails with 502.
- A failure after the head cannot be retried or fall back; the client sees the body end early. A failure before the head follows the buffered rules.
- A `command` keeps its slot and process until the body ends or the client disconnects. Error statuses are read whole for classification, so 401 and 429 still fail over. A forwarder can acknowledge work with 202.

### Failures

Any failure of the link answers 502: failed connect or start, wrong uid, a close without response, a malformed or oversized response, a bad status or a timeout. Reiki logs it without key material and does not cool the key, retry on another key or use `fallback`.

### Command specifics

- Reiki spawns the program once per request and speaks HTTP over its stdin and stdout.
- The environment is cleared except `PATH`, `HOME`, `LANG`, `LC_ALL` and names in `pass_env`; `stderr` is discarded.
- The program has 1 second to exit after the response. One still running at the request timeout is killed; child processes are not.
- Reiki does not close stdin after writing, so the program must read by `Content-Length`.
- At most `max_concurrent` copies run at once (default 16); a request over the cap gets 502. An absolute program path is preferred.

### Socket specifics

- Reiki connects to an existing listener; it never creates, chmods or deletes the socket. Unix only, absolute path, at most 103 bytes.
- With `expect_uid`, Reiki checks the peer's uid before writing.
- Startup refuses a socket in a group- or world-writable directory without a sticky bit unless `expect_uid` is set. `reiki check` warns about a missing socket or such a directory. Test with `curl --unix-socket PATH http://localhost/...`.

### Limits and restrictions

`args`, `pass_env` and `max_concurrent` need `command`; `expect_uid` needs `socket`. A forward upstream cannot use `http2` or `title_case_headers`. It can serve an intercepted tunnel host: Reiki terminates client TLS, swaps the key and hands the request to the forwarder. Without `streaming`, the 120-second timeout bounds the whole exchange; with it, only the wait for the head.

## OpenAI

```toml
[upstream.openai]
base_url = "https://api.openai.com/v1"
```

Client base URL: `http://127.0.0.1:4097/openai`. A 401 takes the key out of rotation and moves the request to the next key. A 429 whose body names a quota follows the quota rules; other 429s follow the rate-limit rule. See [failover](failover.md).

## OpenCode Zen

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
```

Client base URL: `http://127.0.0.1:4097/zen`. Set `confirm_quota = true` for a transient quota 429 that a good key sometimes returns: the first 429 waits 3 seconds and retries the same key, and only a repeated one cools it. A paid-model 402 and a plain rate-limit 429 are scoped to the route and model; a quota 429 cools the whole key. `scope_escalate_after` controls escalation. A 403 "Model access is disabled" tries the next key without disabling it. Reiki forwards the client's headers, so free-model requests keep the headers the service expects. See [failover](failover.md).

## Anthropic

```toml
[upstream.anthropic]
base_url = "https://api.anthropic.com"
auth_header = "x-api-key"
auth_format = "{key}"
```

Client base URL: `http://127.0.0.1:4097/anthropic`. The SDK adds `/v1/messages` and its `anthropic-version` header. A 400 "credit balance is too low" counts as quota exhaustion.

## Gemini

```toml
[upstream.gemini]
base_url = "https://generativelanguage.googleapis.com"
auth_header = "x-goog-api-key"
auth_format = "{key}"
```

Client base URL: `http://127.0.0.1:4097/gemini`. An invalid key is a 400 with `API_KEY_INVALID`. A client that sends the key as `?key=` is accepted when no credential header is present, and the parameter is removed before forwarding.

## Other providers

Any HTTP API that takes its key in a header can be an upstream. For OpenAI-compatible services use the OpenAI block with the service's base URL:

```toml
[upstream.openrouter]
base_url = "https://openrouter.ai/api/v1"
```

Reiki decides a key failure from the status code, `Retry-After` and error-body phrases. For a service that signals quota or key problems differently, name those phrases in `quota_phrases`, `request_error_phrases` or `rate_limit_phrases` on its upstream. See [failover](failover.md) and the [status table above](#provider-status-and-test-coverage).

## Unauthorized keys and relayed errors

A 401 disables a key at once, but one request disables at most one key. With `hold_unauthorized = true`, the first 401 is held until a later key serves the same request; if no key serves it, no key is disabled. A 403 naming an invalid, expired or revoked credential disables the key; other 403s skip it for that request only. Set `pass_errors = true` to relay every error status unchanged instead: no key is cooled, disabled or replayed, and no fallback runs. Error bodies are relayed with secrets scrubbed and a truncation marker. See [failover](failover.md).

## Falling back to another upstream

```toml
[upstream.zen.fallback]
upstream = "openrouter"
model_aliases = { "*" = "openrouter/free" }
```

When no `zen` key in the stream's chain can serve, Reiki sends the request to `openrouter`: `/zen/chat/completions` becomes `/chat/completions` on the target. The fallback runs once, so the target's own fallback is not followed. Model aliases apply in order: the fallback's aliases first, then the target's own aliases and dropped params; `"*"` matches any model. Every stream using the source upstream must hold a key for the target. The request log names the serving key, so the target's key shows that a fallback happened. See [failover](failover.md).

## Passing the client's own credentials through

```toml
[upstream.zen-direct]
base_url = "https://opencode.ai/zen/v1"
passthrough = true
http2 = true
forward_accept_encoding = true
```

A request to `/zen-direct/chat/completions` goes to the provider's URL with the client's headers and query string unchanged, including any `key=` parameter. Reiki checks no stream token and uses no key, so anything that can reach the listener can use the upstream with its own credentials; keep the listener on loopback or behind your own access control. There is no failover, and a passthrough upstream cannot have `fallback`. Plugins still run; the log names the key `-`. Only hop-by-hop headers, `Host` and `Content-Length` are dropped. Reiki still terminates TLS and opens its own connection. Classified passthrough answers and observed rate-limit headers are recorded under the upstream's `account` label; no credential is stored or published. See [failover](failover.md) and [security](security.md).
