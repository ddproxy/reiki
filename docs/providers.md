# Providers

Each provider is an `[upstream.<name>]` table in `reiki.toml`. A client reaches it through Reiki at `http://127.0.0.1:4097/<name>`.

## How a provider maps

- Set `base_url` to the root the provider's own SDK uses by default.
- Set the client's base URL to `http://127.0.0.1:4097/<name>`. The SDK then appends its usual paths, and Reiki forwards them to `base_url`.
- Set `auth_header` and `auth_format` to the way the provider expects the key. The defaults are `authorization` and `Bearer {key}`.
- Clients send their stream token in `Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or a `?key=` query parameter, whichever their SDK uses for the key. The headers win; the query parameter is the last resort.

One stream can chain keys from several providers. A request only uses keys from the provider named in its path.

## Rewriting the request body

Some providers name models differently, and some reject parameters others accept. Two optional upstream settings adjust the JSON body before Reiki sends it:

- `model_aliases = { "gpt-4o" = "opencode/gpt-4o" }` replaces the top-level `model` string when it matches a key.
- `drop_params = ["stream_options"]` removes the listed top-level keys.

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
model_aliases = { "gpt-4o" = "opencode/gpt-4o" }
drop_params = ["stream_options"]
```

Reiki rewrites the body once before it picks a key, so a failover to the next key replays the same bytes. A body that is not a JSON object, a missing or non-string `model`, an unknown model, and a non-matching `drop_params` all pass through byte-for-byte unchanged. Only top-level keys are dropped; nested objects are left alone.

## Transport options

By default Reiki talks to every upstream over HTTP/1.1, asks for uncompressed responses and sends lowercase header names. Four optional upstream settings make the relay behave more like a plain pass-through:

- `http2 = true` negotiates HTTP/2 with that upstream when it offers it over TLS. Without TLS there is nothing to negotiate and the connection stays HTTP/1.1.
- `forward_accept_encoding = true` sends the client's own `Accept-Encoding` upstream instead of `identity`; a client that sent none still gets `identity`. Reiki decodes gzip, deflate, brotli and zstd responses so error bodies can be read for failover and logging, and the client gets plain bytes without `Content-Encoding`. A forward upstream relays `content-encoding` as-is.

- `title_case_headers = true` sends header names in Title-Case (`Content-Type`, `X-Request-Id`) on HTTP/1.1. HTTP/2 always lowercases them, so it has no effect with `http2 = true`.
- `send_keep_alive = true` adds `Connection: keep-alive`. HTTP/1.1 connections persist without it, and Reiki never forwards the client's own `Connection` header because it only describes the client's connection.

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
http2 = true
forward_accept_encoding = true
```

The credential header is always sent first, ahead of the client's own headers. These settings apply to the reverse-proxy routes. Traffic through an intercepted tunnel is relayed as the client sent it; see [tunnel](tunnel.md).

These change how Reiki connects, nothing more. Reiki does not add, rewrite or invent client-identifying headers, and the settings do not change which requests an upstream accepts.

## Forwarding

A forward upstream hands each request to a listener or a program instead of sending it over the network itself. The listener or program can pass it on to another process, a queue, another harness or a proxy.

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

Set exactly one of `socket` or `command`. Add `streaming = true` to relay the answer as it arrives (see below).

### What the forwarder receives

Reiki is the HTTP client and speaks plain HTTP/1.1 with nothing custom. The request line uses the path and query of the upstream URL, and `Host` is the upstream host:

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

The client's other headers pass through as for a normal upstream. The key is sent only in the upstream's credential header, with no second copy and no Reiki-specific header. On a `passthrough` upstream the client's own credential is forwarded unchanged.

Client-sent `Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto` and `X-Real-IP` are removed (Reiki sets `X-Forwarded-Proto` itself), and so are the hop-by-hop headers and the headers named by the client's `Connection`.

### What Reiki does with the answer

The status must be 200-599. Unless `streaming` is set, the body is buffered whole: up to 8 MiB, 128 headers and 64 KiB per header value. Chunked responses are joined. Reiki sets `content-length` to the buffered length (HEAD keeps the forwarder's length; 204 and 304 carry none) and relays `content-encoding` without decoding it. `Expect` is not passed on, and hop-by-hop headers are removed. In this mode nothing reaches the client until the forwarder finishes.

### Streaming

`streaming = true` on `[upstream.X.forward]` (socket or command) hands the client the answer as the forwarder produces it, which an event stream such as a chat completion needs:

```toml
[upstream.zen.forward]
command = "/usr/local/bin/bun"
args = ["/usr/local/lib/reiki/bun-forward.ts", "--allow", "opencode.ai"]
streaming = true
```

- The response is returned as soon as its head is in. The forwarder's body then flows through unbuffered, so the 8 MiB cap does not apply.
- The 120-second request timeout bounds only the wait for the response head, not the body.
- A `content-length` the forwarder reports is kept; otherwise the client gets a chunked body. `transfer-encoding` and the other hop-by-hop headers are still removed, and `content-encoding` is still relayed as-is.
- A forwarder that fails after the head has gone out cannot be retried or fall back: the client sees the body end early with an error. A failure before the head is handled as for a buffered answer.
- A `command` keeps its concurrency slot, and its process, until the body ends or the client disconnects. A client that disconnects ends the child after the usual 1-second grace.
- Error statuses are still read whole for key classification, so a streaming forwarder's 401 or 429 fails over as before.

A forwarder's own error status (for example 401, 402, 429 or 5xx) is treated exactly like an upstream returning it, so normal key classification and failover apply. A forwarder can acknowledge and queue work by answering at once with 202; no special mode is needed.

### Failures

Any failure of the link to the forwarder answers the client with 502. That covers a failure to connect or start, a wrong uid, a connection closed without a response, a malformed or oversized response, a bad status and a timeout. Reiki logs it at error level without key material. It does not cool the key, does not retry on the same or another key, and does not use a configured `fallback` upstream.

### Command specifics

- Reiki spawns the program once per request and speaks HTTP over its stdin and stdout.
- The environment is cleared except `PATH`, `HOME`, `LANG`, `LC_ALL` and the names in `pass_env`.
- `stderr` is discarded.
- The response is returned as soon as it is complete. The program then has 1 second to exit before it is killed.
- A program still running when the request times out is killed. Processes it started are not.
- Reiki does not close the program's stdin after writing the request, so the program must read the request using its `Content-Length` and not wait for end of input.
- At most 16 copies of the command run at once. Set `max_concurrent = N` (N at least 1, valid only with `command`) to change it; a request that would exceed it gets 502.
- The command is looked up with Reiki's own `PATH` unless it is an absolute path, so prefer an absolute path.

### Socket specifics

- Reiki connects to an existing listener. It never creates, chmods or deletes the socket.
- The path must be absolute and at most 103 bytes. Unix only.
- With `expect_uid`, Reiki reads the peer's uid after connecting and before writing anything, and refuses with 502 if it differs.
- Keep the socket in a directory that only the Reiki user and the listener can enter, and set `expect_uid`. See [security](security.md). Inside a rootless container or user namespace, the uid is the mapped one.
- `reiki check` and startup warn when the socket does not exist yet or is not a socket. They also warn about a group- or world-writable directory without `expect_uid`, even when the socket does not exist yet.
- Test a listener with `curl --unix-socket PATH http://localhost/...`.

### Limits and restrictions

- `args`, `pass_env` and `max_concurrent` are valid only with `command`. `expect_uid` is valid only with `socket`.
- A forward upstream cannot be combined with `http2` or `title_case_headers`. Startup rejects these. It can serve an intercepted tunnel host: Reiki still terminates the client's TLS and swaps the key, then hands the request to the forwarder.
- Without `streaming`, the 120-second request timeout bounds the whole exchange: connect, send and the full response. With it, the timeout bounds the wait for the response head.
- A buffered request does not necessarily stop when the client disconnects; it ends when the forwarder answers or the timeout fires.

## OpenAI

```toml
[upstream.openai]
base_url = "https://api.openai.com/v1"
```

Client base URL: `http://127.0.0.1:4097/openai`

Verified live with invalid keys: a 401 takes the key out of rotation and the request moves to the next key. A 429 `insufficient_quota` body cools the key for the quota period; other 429s cool it for `Retry-After`. Both are covered by unit tests but not exercised with a real exhausted key.

## OpenCode Zen

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
```

Client base URL: `http://127.0.0.1:4097/zen`. For OpenCode, set `provider.opencode.options.baseURL` to that URL and `apiKey` to the stream token.

Zen sometimes answers a usable key with a quota 429 (`FreeUsageLimitError`) and accepts the very next request. Set `confirm_quota = true` on the upstream to send such a 429 once more on the same key, after a 3-second pause, before cooling it; only a repeated quota 429 takes the key out of rotation. It costs a genuinely exhausted key one extra request and 3 seconds per request.

Zen's free models only accept requests that carry OpenCode's own headers. Reiki forwards every header except the credential, so the free tier keeps working.

A 403 "Model access is disabled" can apply to one endpoint only (OpenCode probes `/responses` before falling back to `/chat/completions`). Reiki tries the next key for that request, leaves the key in rotation, and returns the upstream's answer if no key helps.

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

Client base URL: `http://127.0.0.1:4097/gemini`. The Gemini API reports an invalid key as a 400 with `API_KEY_INVALID`. Reiki treats that as a rejected key. A client that sends the key as a `?key=` query parameter is accepted when no credential header is present, and Reiki removes the parameter before forwarding.

Verified live with an invalid key, including a client authenticating with `x-goog-api-key`.

## Other providers

Any HTTP API that takes its key in a header can be an upstream. For OpenAI-compatible services such as OpenRouter, Groq or Together, use the OpenAI block with the service's base URL:

```toml
[upstream.openrouter]
base_url = "https://openrouter.ai/api/v1"
```

Reiki decides whether a key failed from the status code, the `Retry-After` header and a few phrases in the error body. A provider that signals quota or key problems in a different way may need a new rule in `src/classify.rs`. These services have not been tested.

## Falling back to another upstream

An upstream can name one other upstream to use when none of its keys is available:

```toml
[upstream.zen.fallback]
upstream = "openrouter"
model_aliases = { "*" = "openrouter/free" }
```

When every key for `zen` in the stream's chain is cooling down or has just failed, Reiki sends the same request to `openrouter` instead of answering 503. The path after the upstream name is kept, so `/zen/chat/completions` becomes `/chat/completions` on the target. Later requests skip the cooling keys and go straight to the target until they recover.

- It is one hop. The target's own `fallback` is not followed.
- The body is rewritten for the target: first the fallback's `model_aliases`, then the target's own `model_aliases` and `drop_params`. A `"*"` key matches any model that has no entry of its own, which is how a provider-specific model name is swapped for one the target serves. Without a mapping the model name goes through unchanged.
- It triggers when no key is left to try, whether the keys failed or every one refused the request (for instance a model that is disabled for them). If the target cannot serve the request either, the client gets the target's answer.
- Every stream whose chain uses the source upstream must also list at least one key for the target, or the config is rejected at startup.
- The request log shows the key that served the request, so the target's key name tells you a fallback happened. The `falling back` warning and `reiki_requests_total` for the target upstream count them.
- The endpoint has to exist on the target: a client that calls an endpoint only the source offers gets the target's own error.

## Passing the client's own credentials through

An upstream with `passthrough = true` makes Reiki a plain reverse proxy for it, like an nginx or Traefik egress:

```toml
[upstream.zen-direct]
base_url = "https://opencode.ai/zen/v1"
passthrough = true
http2 = true
forward_accept_encoding = true
```

A request to `/zen-direct/chat/completions` goes to `https://opencode.ai/zen/v1/chat/completions` with the client's headers, `Authorization` and `x-api-key` included, and the `key=` query parameter removed. Plugins may rewrite the body. Whatever comes back is returned as it is, errors too.

- No Reiki token is checked, so a client needs none. Anything that can reach the listener can use the upstream with its own credentials, so keep the listener on loopback or behind your own access control.
- No key from the chain is used and there is no failover. Keys and streams cannot refer to a passthrough upstream, and it cannot have a `fallback`.
- Plugins still run, so the request log and the placeholders plugin work. The log names the key `-`.
- Only hop-by-hop headers, `Host` and `Content-Length` are dropped. `Accept-Encoding` is forwarded when the upstream sets `forward_accept_encoding`, and replaced with `identity` otherwise.
- Reiki still terminates TLS and opens its own connection, as any reverse proxy does, so the upstream sees Reiki's TLS handshake and HTTP version rather than the client's.
