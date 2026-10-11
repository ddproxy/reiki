# Providers

Each provider is an `[upstream.<name>]` table in `reiki.toml`. A client reaches it through Reiki at `http://127.0.0.1:4097/<name>`.

## How a provider maps

- Set `base_url` to the root the provider's own SDK uses by default.
- Set the client's base URL to `http://127.0.0.1:4097/<name>`. The SDK then appends its usual paths, and Reiki forwards them to `base_url`.
- Set `auth_header` and `auth_format` to the way the provider expects the key. The defaults are `authorization` and `Bearer {key}`.
- Clients send their stream token in `Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or a `?key=` query parameter, whichever their SDK uses for the key. The headers win; the query parameter is the last resort.

One stream can chain keys from several providers. A request only uses keys from the provider named in its path.

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

Verified live with invalid keys. A 400 "credit balance is too low" counts as quota exhaustion, from Anthropic's documented error body.

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
