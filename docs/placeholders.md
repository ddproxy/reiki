# Placeholders plugin

`placeholders` keeps sensitive values out of requests Reiki sends upstream: it swaps credential-shaped strings and chosen terms for stable placeholders on the way out and restores the originals in the response. The request log, which runs after it, records only masked traffic.

Compiled in by the default `plugin-placeholders` feature; inactive until a `[plugins.placeholders]` table exists.

## Configure

```toml
[plugins.placeholders]
terms = ["Project Falcon", "Acme Corp"]      # literal terms, any case
terms_file = "/etc/reiki/terms.txt"          # more terms, one per line, # for comments
builtin = true                               # also mask credential-shaped strings
rehydrate = true                             # restore the originals in responses
max_response_bytes = 8388608                 # longest non-streaming body held to restore it
```

| Key | Default | Meaning |
| --- | --- | --- |
| `terms` | none | Literal terms to mask. Case is ignored and the first spelling seen is what comes back. Each must be at least 3 characters and contain no quote, backslash or control character. |
| `terms_file` | none | Absolute path to a file of terms, one per line. Blank lines and lines starting with `#` are skipped. Read at startup and again on a reload that finds the file changed. Keep it mode 0600; client and project names are often as private as the values they stand for. |
| `builtin` | `true` | Also mask the credential shapes the request log masks: common API key prefixes, `Bearer` tokens, JWTs. A PEM private key block is removed outright and is never restored. |
| `rehydrate` | `true` | Put the originals back in responses. With `false` the model's placeholders reach the client as written. |
| `restore_tool_args` | `false` | Also restore credentials inside tool-call arguments. Off, a credential placeholder inside a tool call stays masked in the response. Terms are always restored. |
| `max_response_bytes` | 8 MiB | A response that is not an event stream is held whole to restore it. A larger one is released unchanged. |

At least one of `terms`, `terms_file` or `builtin` must leave something to mask. At startup a bad value, an unknown key or an unreadable `terms_file` stops Reiki from starting. `reiki check` validates the table without reading the terms file.

A [reload](troubleshooting.md#reload) re-reads `terms_file` when the file changed. The plugin is rebuilt, but the salt is shared by the whole process, so placeholder ids stay the same and the provider's prompt cache is not invalidated. Requests started before the reload finish with the old plugin instance and the same ids.

## What it does

A masked value becomes `[[REIKI_TERM_9b2e41f0c3a7d652]]` (a term) or `[[REIKI_SECRET_3f9a1c07e5b08d41]]` (a credential). The id depends only on the value and a random salt that lasts as long as the process, so the same value gets the same placeholder in every request, including after a reload; a resent history masks the same way and the provider's prompt cache keeps working. A restart changes every id, so the first request after it misses the cached prefix.

Reasoning blocks are not touched. A `thinking` block's `thinking` text and `signature`, and a `redacted_thinking` block's `data`, pass through byte for byte in both directions, including streamed `thinking_delta` and `signature_delta` events. Base64 `image` and `document` blocks pass through untouched and are never scanned. Treat anything inside those blocks as already sent, not as protected.

Responses are restored as they stream; JSON and other bodies are held until they end, then restored. A JSON body carrying tool calls is walked as JSON so credential placeholders can stay masked in them (see `restore_tool_args`); only changed string values are rewritten, so key order, spacing and number formatting survive byte for byte.

For event streams (`text/event-stream`), each event is parsed. A placeholder split across two events, such as `[[REI` then `KI_TERM_9b2e41f0c3a7d652]]`, is held back from the first and completed with the second, and a lone `[` at the end of a field is held the same way. A held fragment still unfinished when the stream ends, at `[DONE]`, at `message_stop` or at the last chunk, is sent as plain text. Events with no placeholder start keep their exact bytes; the others are re-serialised as JSON, so key order and number formatting can change.

Plugin order is set at build time (see [plugins](plugins.md#choose-and-order-plugins-at-build-time) and [plugin authoring](plugin-authoring.md#build-time-registration)); the default build registers `placeholders` before `requestlog`. Request hooks run in that order, so the log sees the masked request. Response hooks run in reverse, so the log sees the masked response and the originals are restored only afterwards.

## Claude Code

Point Claude Code at a Reiki upstream that proxies the Anthropic Messages API:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080/anthropic
claude
```

The path segment after the host selects the upstream, so `[upstream.anthropic] base_url = "https://api.anthropic.com"` receives `/v1/messages`. A Claude Pro/Max login uses OAuth: make that upstream `passthrough = true` and Reiki forwards the `authorization` and `x-api-key` headers untouched, without a `[streams]` entry. For an Anthropic API key managed by Reiki, configure `[keys]` and a `[streams]` entry whose token Claude Code sends as `x-api-key`; Reiki swaps in the real upstream key.

The plugin masks message text, array-of-blocks text, `tool_result` content, `system` (string or array), tool descriptions and `stop_sequences`, and restores placeholders across streamed text and tool-argument deltas. Thinking blocks, signatures and base64 images or PDFs are relayed byte for byte (see [What it does](#what-it-does)).

What is guaranteed: no configured term or detected credential in scannable request text reaches Anthropic; a resent history masks to the same placeholder ids while the process lives, including across a reload; the response is restored before Claude Code sees it.

What is not: request headers, paths and query strings are not masked, OAuth headers pass through as they are, and image, PDF and thinking payloads are never scanned. Restarting Reiki changes every placeholder id and loses the provider's cached prefix. A model that paraphrases a value instead of repeating its placeholder defeats the restore, and a stream that ends mid-placeholder delivers the unfinished fragment as plain text.

## Limits

- This is pattern and list matching, not a data-loss guarantee. Credential detection is heuristic, and a term matches only the exact characters you list.
- Only the request body is masked, as raw text: header values, the path and query string are not, and JSON object keys are masked like any other text.
- Base64 `image` and `document` (PDF) blocks cannot be scanned and pass through untouched. The same applies to `thinking`, `signature` and `redacted_thinking` payloads, which must stay byte-identical for the provider: a secret written into model reasoning is sent upstream.
- A model can paraphrase or transform a value instead of repeating the placeholder; whatever it writes back that is not a placeholder is delivered as it is.
- A request body with a `Content-Encoding` other than `identity`, or that is not valid UTF-8 text, cannot be masked and is refused with 415 rather than sent raw. `Content-Encoding: identity` is a no-op and does not prevent masking.
- A response with a `Content-Encoding` other than `identity` is not restored, and a stream that ends mid-placeholder sends that unfinished fragment as plain text, unrestored.
- Matching is by substring, so a term inside a longer word is masked too. Choose terms accordingly.
