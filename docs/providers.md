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
- `forward_accept_encoding = true` sends the client's own `Accept-Encoding` header upstream instead of `identity`. A client that sent none still gets `identity`. Reiki decodes gzip, deflate, brotli and zstd responses itself, so error bodies are still read for key failover and logging, and the client receives plain bytes without a `Content-Encoding` header.

- `title_case_headers = true` sends header names in Title-Case (`Content-Type`, `X-Request-Id`) on HTTP/1.1. HTTP/2 always lowercases them, so it has no effect with `http2 = true`.
- `send_keep_alive = true` adds `Connection: keep-alive`. HTTP/1.1 connections persist without it, and Reiki never forwards the client's own `Connection` header because it only describes the client's connection.

```toml
[upstream.zen]
base_url = "https://opencode.ai/zen/v1"
http2 = true
forward_accept_encoding = true
```

The credential header is always sent first, ahead of the client's own headers.

These change how Reiki connects, nothing more. Reiki does not add, rewrite or invent client-identifying headers, and the settings do not change which requests an upstream accepts.

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

A request to `/zen-direct/chat/completions` goes to `https://opencode.ai/zen/v1/chat/completions` with every header the client sent, `Authorization` and `x-api-key` included, and the body as sent. Whatever comes back is returned as it is, errors too.

- No Reiki token is checked, so a client needs none. Anything that can reach the listener can use the upstream with its own credentials, so keep the listener on loopback or behind your own access control.
- No key from the chain is used and there is no failover. Keys and streams cannot refer to a passthrough upstream, and it cannot have a `fallback`.
- Plugins still run, so the request log and the placeholders plugin work. The log names the key `-`.
- Only hop-by-hop headers, `Host` and `Content-Length` are dropped. `Accept-Encoding` is forwarded when the upstream sets `forward_accept_encoding`, and replaced with `identity` otherwise.
- Reiki still terminates TLS and opens its own connection, as any reverse proxy does, so the upstream sees Reiki's TLS handshake and HTTP version rather than the client's.
