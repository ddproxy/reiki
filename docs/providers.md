# Providers

Each provider is an `[upstream.<name>]` table in `reiki.toml`. A client reaches it through Reiki at `http://127.0.0.1:4097/<name>`.

## How a provider maps

- Set `base_url` to the root the provider's own SDK uses by default.
- Set the client's base URL to `http://127.0.0.1:4097/<name>`, so the SDK appends its usual paths and Reiki forwards them to `base_url`.
- Set `auth_header` and `auth_format` to the provider's expectation (defaults `authorization` and `Bearer {key}`); a bad pair is refused when the config loads.
- Clients send their stream token in `Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or a `?key=` query parameter, whichever their SDK uses. Headers win; the query parameter is last.

A stream can chain keys from several providers; a request uses only keys from the provider in its path.

## Rewriting the request body

Two optional upstream settings adjust the JSON body before Reiki sends it:

- `model_aliases = { "gpt-4o" = "opencode/gpt-4o" }` replaces the top-level `model` string when it matches a key.
- `drop_params = ["stream_options"]` removes the listed top-level keys.

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
model_aliases = { "gpt-4o" = "opencode/gpt-4o" }
drop_params = ["stream_options"]
```

Reiki rewrites the body once before picking a key, so failover replays the same bytes. A non-object body, a missing or non-string `model`, an unknown model and a non-matching `drop_params` pass through byte-for-byte. Only top-level keys are dropped; nested objects stay. Keys and the `model` value are compared after JSON escapes are decoded, so `{"model":"gpt\u002d4o"}` matches an alias for `gpt-4o`; otherwise they must match exactly.

## Transport options

By default Reiki uses HTTP/1.1, asks for uncompressed responses and sends lowercase header names. Optional settings make the relay more of a plain pass-through:

- `http2 = true` negotiates HTTP/2 when the upstream offers it over TLS; without TLS the connection stays HTTP/1.1.
- `forward_accept_encoding = true` sends the client's `Accept-Encoding` upstream instead of `identity`; a client that sent none still gets `identity`. Reiki decodes gzip, deflate, brotli and zstd so error bodies can be read for failover and logging; the client gets plain bytes without `Content-Encoding`. A forward upstream relays `content-encoding` as-is.
- `max_decoded_bytes` caps a body after decoding, compressed responses included. Default 64 MiB, at least 1. A decoded non-SSE response over the cap becomes a 502 before anything reaches the client; an SSE response that passes the cap ends with an error.
- `response_idle_timeout_secs` drops a response body with no chunk for that many seconds. Default 300, at least 1. A buffered decoded body answers 504; a streamed body ends with an error.
- `response_timeout_secs` bounds the whole body from its head. Unset by default, so only the idle limit applies; at least 1 when set. The separate 120-second request timeout bounds the wait for the head.
- `title_case_headers = true` sends Title-Case header names (`Content-Type`, `X-Request-Id`) on HTTP/1.1; HTTP/2 always lowercases, so it does nothing with `http2 = true`.
- `send_keep_alive = true` adds `Connection: keep-alive`. HTTP/1.1 connections persist without it, and Reiki never forwards the client's `Connection` header, which only describes the client's connection. A forward upstream always closes, so this is refused there.

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
http2 = true
forward_accept_encoding = true
```

The credential header goes first, ahead of the client's headers. These settings apply to reverse-proxy routes; an intercepted tunnel relays traffic as the client sent it (see [tunnel](tunnel.md)).

They change how Reiki connects, not which requests an upstream accepts, and Reiki never adds, rewrites or invents client-identifying headers.

## Outbound limits

An upstream with a `limits` table gives each key an outbound budget; without one keys are never shaped:

```toml
[upstream.zen.limits]
preset = "zen-free"          # or "openrouter-free"
rpm = 20                     # requests per minute
tpm = 100000                 # tokens per minute (initial estimate: body bytes / 4)
concurrent = 2               # requests in flight
daily_requests = 50
daily_tokens = 250000
max_wait_secs = 20           # longest Reiki waits for a slot, 1-120
```

Every field is optional; an explicit `0` means unlimited, even over a preset. A preset seeds the values and explicit fields override them; `min_rpm` sets a floor for learning.

Reiki takes a slot for the chosen key before sending. A wait within `max_wait_secs` is slept off; a longer wait or a closed daily quota skips that key for this request and tries the next. If no key is usable, the client gets `429` with JSON error code `reiki_shaped`, `Retry-After` and `x-reiki-origin: local`. A `concurrent` slot is held until the response body ends or the client disconnects.

The first token charge is an estimate of `body bytes / 4`. When the response reports provider `usage` in JSON or an SSE event, Reiki replaces it with the prompt + completion count as the body streams, so `tpm` and the daily and monthly token counters include output. A body with no usage, or a compressed body, keeps the estimate.

A response from that key's own upstream teaches the bucket: `Retry-After` on a rate-limit `429` pauses it, `402`/quota exhaustion closes it for the key cooldown, and `remaining: 0` closes it until the reported reset. The learned cap, remaining pause or closed time and current daily and monthly counters persist in the state file across restarts; the sliding minute log is not saved, and a key whose file or upstream changed starts with a fresh bucket.

## Forwarding

A forward upstream hands each request to a listener or a program instead of sending it itself; that process can pass it on to another process, a queue, another harness or a proxy.

Socket target, an already-running listener:

```toml
[upstream.example.forward]
socket = "/run/forwarder.sock"
expect_uid = 1000
```

Command target, a program started per request:

```toml
[upstream.example.forward]
command = "/usr/local/bin/forward-request"
args = ["--config", "/etc/forward.toml"]
pass_env = ["FORWARD_TARGET"]
```

Set exactly one of `socket` or `command`; add `streaming = true` to relay the answer as it arrives (see below).

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

The other client headers pass through as for a normal upstream. The key is sent only in the upstream's credential header, with no second copy and no Reiki-specific header; a `passthrough` upstream forwards the client's own credential unchanged.

Client-sent `Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto` and `X-Real-IP` are removed (Reiki sets `X-Forwarded-Proto`), as are hop-by-hop headers and headers named by the client's `Connection`.

### What Reiki does with the answer

The status must be 200-599. Without `streaming`, the body is buffered whole: up to 8 MiB, 128 headers and 64 KiB per header value; chunked responses are joined. Reiki sets `content-length` to the buffered length (HEAD keeps the forwarder's length; 204 and 304 carry none) and relays `content-encoding` without decoding it. `Expect` is not passed on and hop-by-hop headers are removed. Nothing reaches the client until the forwarder finishes.

### Streaming

`streaming = true` on `[upstream.X.forward]` (socket or command) hands the client the answer as it is produced, which event streams need:

```toml
[upstream.zen.forward]
command = "/usr/local/bin/bun"
args = ["/usr/local/lib/reiki/forwarder.ts", "--allow", "opencode.ai"]
streaming = true
```

- The response returns as soon as its head is in; the body then flows unbuffered, so the 8 MiB cap does not apply.
- The 120-second request timeout bounds only the wait for the head, not the body.
- A reported `content-length` is kept; otherwise the body is chunked. `transfer-encoding` and the other hop-by-hop headers are removed, and `content-encoding` is still relayed as-is.
- A forwarder that fails after the head cannot be retried or fall back: the client sees the body end early with an error. A failure before the head is handled as for a buffered answer.
- A `command` keeps its concurrency slot and process until the body ends or the client disconnects; a disconnecting client ends the child after the usual 1-second grace.
- Error statuses are read whole for key classification, so a streaming forwarder's 401 or 429 fails over as before.
- A forwarder's own error status (for example 401, 402, 429 or 5xx) is treated like an upstream returning it, so normal classification and failover apply. A forwarder can acknowledge and queue work by answering at once with 202; no special mode is needed.

### Failures

Any failure of the link answers 502: a failed connect or start, a wrong uid, a connection closed without a response, a malformed or oversized response, a bad status or a timeout. Reiki logs it at error level without key material, does not cool the key, does not retry on the same or another key, and does not use a configured `fallback`.

### Command specifics

- Reiki spawns the program once per request and speaks HTTP over its stdin and stdout.
- The environment is cleared except `PATH`, `HOME`, `LANG`, `LC_ALL` and the names in `pass_env`.
- `stderr` is discarded.
- The response returns when complete; the program then has 1 second to exit before it is killed.
- A program still running at the request timeout is killed; processes it started are not.
- Reiki does not close stdin after writing the request, so the program must read it by `Content-Length`, not wait for end of input.
- At most 16 copies run at once. Set `max_concurrent = N` (N at least 1, valid only with `command`) to change it; a request over it gets 502.
- The command is looked up with Reiki's `PATH` unless it is absolute, so prefer an absolute path.

### Socket specifics

- Reiki connects to an existing listener and never creates, chmods or deletes the socket.
- The path must be absolute and at most 103 bytes. Unix only.
- With `expect_uid`, Reiki reads the peer's uid after connecting, before writing anything, and refuses with 502 on a mismatch.
- Keep the socket in a directory only the Reiki user and listener can enter, and set `expect_uid` (see [security](security.md)). In a rootless container or user namespace, the uid is the mapped one.
- `reiki check` and startup warn when the socket is missing or is not a socket, and about a group- or world-writable directory without `expect_uid`, even when the socket does not exist yet.
- Test with `curl --unix-socket PATH http://localhost/...`.

### Limits and restrictions

- `args`, `pass_env` and `max_concurrent` need `command`; `expect_uid` needs `socket`.
- A forward upstream cannot be combined with `http2` or `title_case_headers`; startup rejects both. It can serve an intercepted tunnel host: Reiki terminates the client's TLS and swaps the key, then hands the request to the forwarder.
- Without `streaming`, the 120-second request timeout bounds the whole exchange: connect, send and the full response; with it, only the wait for the head. `response_idle_timeout_secs` and `response_timeout_secs` also bound the body.
- A buffered request does not necessarily stop when the client disconnects; it ends when the forwarder answers or the timeout fires.

## OpenAI

```toml
[upstream.openai]
base_url = "https://api.openai.com/v1"
```

Client base URL: `http://127.0.0.1:4097/openai`

Covered by tests with invalid keys: a 401 takes the key out of rotation and moves the request to the next key. A 429 `insufficient_quota` body cools the key for the quota period; other 429s cool it for `Retry-After`. Both are unit-tested but not exercised with a real exhausted key.

## OpenCode Zen

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
```

Client base URL: `http://127.0.0.1:4097/zen`. For OpenCode, set `provider.opencode.options.baseURL` to that URL and `apiKey` to the stream token.

Some providers answer a usable key with a transient quota 429 (`FreeUsageLimitError`) and accepts the next request. Set `confirm_quota = true` to send such a 429 once more on the same key after a 3-second pause before cooling it; only a repeated quota 429 takes the key out of rotation. A genuinely exhausted key pays one extra request and 3 seconds per request.

A free-tier account answers a paid model with 402 while its free models keep working, so a whole-key cooldown would take them down too. A 402 and a plain rate-limit 429 are scoped to the request's route and model: the key keeps serving other combinations, and `/status` lists the cooling combination under `scopes`. A quota 429 still cools the whole key, because it concerns the account, not one model. `scope_escalate_after` (default 3, 2 to 16) sets how many models may be cooling before another failure cools the key.

Zen's free models only accept requests carrying OpenCode's own headers; Reiki forwards every header except the credential, so the free tier keeps working.

A 403 "Model access is disabled" can apply to one endpoint only (OpenCode probes `/responses` before `/chat/completions`). Reiki tries the next key for that request, leaves the key in rotation, and returns the upstream's answer if no key helps.

## Anthropic

```toml
[upstream.anthropic]
base_url = "https://api.anthropic.com"
auth_header = "x-api-key"
auth_format = "{key}"
```

Client base URL: `http://127.0.0.1:4097/anthropic`. The SDK adds `/v1/messages` and its `anthropic-version` header, and Reiki passes both through.

A 400 "credit balance is too low" counts as quota exhaustion, from Anthropic's documented error body.

## Gemini

```toml
[upstream.gemini]
base_url = "https://generativelanguage.googleapis.com"
auth_header = "x-goog-api-key"
auth_format = "{key}"
```

Client base URL: `http://127.0.0.1:4097/gemini`. The Gemini API reports an invalid key as a 400 with `API_KEY_INVALID`; Reiki treats that as a rejected key. A client that sends the key as a `?key=` query parameter is accepted when no credential header is present, and Reiki removes the parameter before forwarding.

Covered by tests with an invalid key, including a client authenticating with `x-goog-api-key`.

## Other providers

Any HTTP API that takes its key in a header can be an upstream. For OpenAI-compatible services such as OpenRouter, Groq or Together, use the OpenAI block with the service's base URL:

```toml
[upstream.openrouter]
base_url = "https://openrouter.ai/api/v1"
```

Reiki decides a key failure from the status code, the `Retry-After` header and a few error-body phrases. A provider that signals quota or key problems differently may need a new rule in `src/classify.rs`. These services are untested.

## Unauthorized keys

A 401 takes the key out of rotation at once, and one request disables at most one key: a second key that refuses the same request is tried but left alone. A request that every key refuses (a malformed credential header, a model the account cannot use) therefore still costs one good key. Set `hold_unauthorized = true` to hold that disable back until a later key serves the same request; only then is the first refusing key disabled. If every key of the upstream refuses, or the request ends in a fallback upstream or an error, no key is disabled. The default is `false`. While a bad key waits for a request to succeed, each request that reaches it pays one extra attempt.

## Relaying errors as they are

Reiki reads a 401, a 403 or a quota phrase as a key failure and tries the next key. A 401, or a 403 whose body names an invalid, expired or revoked credential, disables the key; any other 403 skips the key for that request only. That is wrong for an upstream where those statuses mean something else, such as a git server that answers 403 to a read-only token that tries to push. Set `pass_errors = true` to relay every error status to the client as it is. No key is cooled, disabled or replayed, and the key's failover and `fallback` do not apply.

## Falling back to another upstream

An upstream can name one other upstream to use when none of its keys is available:

```toml
[upstream.zen.fallback]
upstream = "openrouter"
model_aliases = { "*" = "openrouter/free" }
```

When every `zen` key in the stream's chain is cooling or has just failed, Reiki sends the same request to `openrouter` instead of answering 503; `/zen/chat/completions` becomes `/chat/completions` on the target. Later requests skip the cooling keys and go straight to the target until they recover.

- It is one hop: the target's own `fallback` is not followed.
- The body is rewritten for the target: first the fallback's `model_aliases`, then the target's own `model_aliases` and `drop_params`. A `"*"` key matches any model without an entry of its own, which swaps a provider-specific name for one the target serves; without a mapping the name is unchanged.
- It triggers when no key is left to try, whether they failed or all refused the request (for instance a model disabled for them); if the target cannot serve it either, the client gets the target's answer.
- Every stream whose chain uses the source upstream must list at least one key for the target, or startup rejects the config.
- The request log names the key that served the request, so the target's key tells you a fallback happened. The `falling back` warning and the target upstream's `reiki_requests_total` count them.
- The endpoint must exist on the target: a client that calls one only the source offers gets the target's own error.

## Passing the client's own credentials through

An upstream with `passthrough = true` makes Reiki a plain reverse proxy for it, like an nginx or Traefik egress:

```toml
[upstream.zen-direct]
base_url = "https://opencode.ai/zen/v1"
passthrough = true
http2 = true
forward_accept_encoding = true
```

A request to `/zen-direct/chat/completions` goes to `https://opencode.ai/zen/v1/chat/completions` with the client's headers, `Authorization` and `x-api-key` included, and its query string unchanged, including any `key=` parameter. Plugins may rewrite the body. Whatever comes back is returned as it is, errors too.

- No Reiki token is checked, so a client needs none. Anything that can reach the listener can use the upstream with its own credentials, so keep the listener on loopback or behind your own access control.
- No key from the chain is used and there is no failover. Keys and streams cannot refer to a passthrough upstream, and it cannot have a `fallback`.
- Plugins still run, so the request log and the placeholders plugin work. The log names the key `-`.
- Only hop-by-hop headers, `Host` and `Content-Length` are dropped. `Accept-Encoding` is forwarded when the upstream sets `forward_accept_encoding`, and replaced with `identity` otherwise.
- Reiki still terminates TLS and opens its own connection, as any reverse proxy does, so the upstream sees Reiki's TLS handshake and HTTP version rather than the client's.
