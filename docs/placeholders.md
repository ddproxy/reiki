# Placeholders plugin

The `placeholders` plugin keeps sensitive values out of the requests Reiki sends upstream. It swaps credential-shaped strings and a list of terms you choose for stable placeholders on the way out, and puts the originals back into the response before the client sees it. The request log, which runs after it, only ever records the masked traffic.

It is a bundled plugin, compiled in by the default `plugin-placeholders` feature and inactive until a `[plugins.placeholders]` table exists. Build with `--no-default-features` to leave it out.

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
| `terms_file` | none | Absolute path to a file of terms, one per line. Blank lines and lines starting with `#` are skipped. Read at startup. Keep it mode 0600; client and project names are often as private as the values they stand for. |
| `builtin` | `true` | Also mask the credential shapes the request log masks: common API key prefixes, `Bearer` tokens, JWTs. A PEM private key block is removed outright and is never restored. |
| `rehydrate` | `true` | Put the originals back in responses. With `false` the model's placeholders reach the client as written. |
| `restore_tool_args` | `false` | Also restore credentials inside tool-call arguments. Off, a credential placeholder inside a tool call stays masked in the response. Terms are always restored. |
| `max_response_bytes` | 8 MiB | A response that is not an event stream is held whole to restore it. A larger one is released unchanged. |

At least one of `terms`, `terms_file` or `builtin` must leave something to mask. At startup a bad value, an unknown key or an unreadable `terms_file` stops Reiki from starting. `reiki check` validates the table without reading the terms file.

## What it does

A masked value becomes `[[REIKI_TERM_9b2e41ff]]` (a term) or `[[REIKI_SECRET_3f9a1c07]]` (a credential). The id depends only on the value and a random salt that lasts as long as the process, so the same value gets the same placeholder in every request. A conversation that resends its history therefore masks the same way each time, and the provider's prompt cache keeps working. Restarting Reiki changes every id.

Responses are restored as they stream. JSON and other bodies are held until they end, then restored. For event streams (`text/event-stream`) each event is parsed, and a placeholder that a model splits across two events, such as `[[REI` then `KI_TERM_9b2e41ff]]`, is held back from the first event and completed with the second. Events that contain no `[[` keep their exact bytes; the others are re-serialised as JSON, so key order and number formatting can change.

The order of plugins matters and is fixed: `placeholders` is registered before `requestlog`. Request hooks run in that order, so the log sees the masked request. Response hooks run in reverse, so the log sees the masked response, and the original values are restored only after that.

## Limits

- This is pattern and list matching, not a data-loss guarantee. Credential detection is heuristic, and a term only matches the exact characters you list.
- Only the request body is masked, as raw text: header values, the path and the query string are not, and JSON object keys are masked like any other text.
- A model can paraphrase or transform a value instead of repeating the placeholder. Whatever it writes back that is not a placeholder is delivered as it is.
- A request body that is encoded (`Content-Encoding`) or not UTF-8 text cannot be masked and is refused with 415 rather than sent raw.
- A response with a `Content-Encoding` is not restored, and a stream that ends in the middle of a placeholder loses that fragment.
- Matching is by substring, so a term inside a longer word is masked too. Choose terms accordingly.
