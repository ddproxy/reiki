# Troubleshooting

## Check the setup

```bash
reiki check
```

`reiki check` parses the config, reads every key file and prints the number of upstreams, keys and streams. It exits 0 when the setup is valid. On failure it prints the problem to stderr and exits 1.

## Status codes from Reiki

These come from Reiki itself. Any other status is the upstream's answer, passed through.

| Status | Body `error.type` and message | Cause |
| --- | --- | --- |
| 401 | `unauthorized`: `missing token` | The request has no stream token |
| 401 | `unauthorized`: `unknown token` | No stream has a matching `token_sha256` |
| 404 | `not_found`: `no upstream in path` | The path has no upstream prefix |
| 404 | `not_found`: `unknown upstream` | The prefix does not match an `[upstream.*]` table |
| 413 | `too_large`: `request body too large` | The body is over 8 MB |
| 503 | `reiki_exhausted` | Every key in the stream's chain is cooling or out of rotation |

A 503 carries `Retry-After` (at least 1 second) when some key will recover. It carries none when every key is out of rotation, because those keys stay out until you replace the key file and send SIGHUP, or restart Reiki.

## Logs

Reiki logs to stderr. Set the level with `RUST_LOG`; the default is `info`.

- `info`: one line per forwarded request with the stream, key name, method, path and upstream status.
- `warn`: a key failed, with its failure class and whether it is now cooling or out of rotation.
- `debug`: the first 160 characters of upstream error bodies. Some providers echo part of the rejected key in these, so keep `debug` off in shared logs.

Key values and stream tokens are never logged.

## Common problems

**503 right after start.** Look for `upstream key failed` lines at `warn`. A key rejected with 401 stays out of rotation until you replace the key file and send SIGHUP, or restart Reiki.

**401 from Reiki.** Hash the token the client sends and compare it with `token_sha256`: `printf '%s' "$TOKEN" | reiki hash-token`.

**404 from the upstream.** The client base URL and `base_url` disagree. The client must use `http://127.0.0.1:4097/<name>`, and `base_url` must be the root the provider's SDK uses. See [providers](providers.md).

**A request fails after two minutes.** Reiki waits 120 s for upstream response headers, then counts a transient failure and tries again.
