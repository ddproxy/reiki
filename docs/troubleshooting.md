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
| 413 | `too_large`: `request body too large` | The body is over 8 MiB |
| 429 | `rate_limited`: `stream request rate limit exceeded` | The stream is over its `rate_limit_per_minute` |
| 429 | Too many failed logins | The client address sent 10 bad tokens in a minute; wait for `Retry-After` |
| 503 | `reiki_exhausted` | Every key in the stream's chain is cooling or out of rotation |

A 429 from Reiki comes from a stream's `rate_limit_per_minute`, carries `Retry-After` in whole seconds, and is decided before any key is picked, so no key enters cooldown. The counter lives in memory and resets on reload.

A 503 carries `Retry-After` (at least 1 second) when some key will recover. It carries none when every key is out of rotation, because those keys stay out until you replace the key file and send SIGHUP.

## Logs

Reiki's own log is separate from the [request log](requestlog.md). By default it goes to stderr at `info`. The `[log]` table changes that:

```toml
[log]
level = "info,reiki::tunnel=debug"   # a level, or a filter per module
file = "/var/log/reiki.log"          # also write here; the service user must be able to write it
format = "text"                      # the file's layout: "text" or "json"
stderr = true                        # keep stderr too; false stops duplicating a service manager's journal
file_max_bytes = 52428800            # rotate when the next line would pass this; 0 never rotates
file_keep = 5                        # rotated files kept as <file>.1 (newest) to <file>.N
```

- `RUST_LOG`, when set, overrides `level` at startup.
- SIGHUP applies a changed `level` without a restart and reopens the file, so an external rotation tool can move it. Changing `file`, `format` or `stderr` needs a restart.
- Every line made while a request is handled carries its `id` in the span prefix (`req{id=…}`). The same `id` is on that request's records in the request log.
- `reiki check` validates the table without opening the file.

What each level shows:

- `info`: one line per forwarded request with the stream, key name, method, path and upstream status, plus tunnel open and close lines.
- `warn`: a key failed, with its failure class, whether it is now cooling and for how many seconds (`cooling_secs`), plus refused tunnel destinations and refused intercepted requests (a `Host` header mismatch or a path outside the intercepted API).
- `debug`: the first 160 characters of upstream error bodies and tunnel I/O errors. Some providers echo part of the rejected key in error bodies, so keep `debug` off in shared logs.

Key values and stream tokens are never logged.

## Reload

Send `SIGHUP` (`kill -HUP <pid>`) or `POST /reload` to the admin listener to apply a changed config without a restart. Both re-read the config and key files, build and check the whole new state first, and swap it in only if every part is valid. A bad config keeps the running one and reports why. Requests and tunnels already open finish on the state they started with.

To reload without a signal, turn on the watcher:

```toml
[reload]
watch = true          # off by default
interval_secs = 5     # 1 to 3600
```

Every interval Reiki checks the config file, the key files and any file a plugin reads. When they have changed and then stayed unchanged for one more interval, it reloads, so a file still being written is never read half done. A config that fails to load is tried once and left alone until a file changes again. `watch` and `interval_secs` themselves apply on the next reload.

Applied live:

- keys, streams, upstreams and the admin token hash
- plugin settings: a plugin whose `[plugins.<name>]` table did not change keeps running, so its counters and open files survive. A changed table builds that plugin again, and adding or removing a table adds or removes the plugin. The `requestlog` plugin holds a file and a socket, so a changed `requestlog` table is reported as needing a restart and the running log is kept.
- files a plugin reads: editing the placeholders `terms_file` and reloading builds that plugin again, even when the config text is the same
- the tunnel `allow` list, `deny_private`, `allow_loopback` and `connect_timeout_secs`
- the intercept host table, `require_auth` and `leaf_ttl_hours`

Needs a restart, and is reported in the result as `restart_needed`:

- every `listen` address (client, admin and tunnel) and the `[log]` file path
- adding or removing `[tunnel]` or `[tunnel.intercept]`
- the CA files: a changed `ca_cert` or `ca_key` refuses the whole reload

The result lists what changed, and the log carries one line per reload. `GET /status` shows `config.generation` (the number of successful reloads), `config.loaded_at` and `config.last_reload`, with the error when the last attempt failed. See [monitoring](monitoring.md#post-reload).

## Key health

Key health is saved next to the config, as `<config>.state`, and restored at startup, so cooldowns and out-of-rotation keys survive a restart. The directory must be writable by the user Reiki runs as; otherwise Reiki logs a warning and runs without persistence. A reload keeps the live health of keys whose files did not change and resets the keys whose files did, and does not read the state file again. The file holds key names and timings only, never a key value or its hash. A key file modified after the state was written counts as rotated and starts healthy (rotation is detected from the file's modification time, so a copy that preserves the old time, such as `cp -p` or `mv` of an old file, is not seen); a missing or unreadable state file is ignored with a warning.

## Common problems

**503 right after start.** Look for `upstream key failed` lines at `warn`. A key rejected with 401 stays out of rotation until you replace the key file and send SIGHUP; the disabled state is saved to `<config>.state` and restored on restart.

**401 from Reiki.** Hash the token the client sends and compare it with `token_sha256`: `printf '%s' "$TOKEN" | reiki hash-token`.

**404 from the upstream.** The client base URL and `base_url` disagree. The client must use `http://127.0.0.1:4097/<name>`, and `base_url` must be the root the provider's SDK uses. See [providers](providers.md).

**A key stays cooling for hours.** A quota-exhausted key cools for 5 h by default. Set `quota_resets` on the key to end its cooldown at the provider's next reset instead; see the config section of the README.

**A request fails after two minutes.** Reiki waits 120 s for upstream response headers, then counts a transient failure and tries again.

**429 from Reiki.** Either the stream hit its `rate_limit_per_minute`, or the client address sent 10 wrong tokens in a minute (look for `failed login` in the log). Wait for `Retry-After`, or raise the limit. Reloading config resets the counter.
