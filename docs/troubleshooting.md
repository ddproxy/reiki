# Troubleshooting

Start with the symptom. Every command below reads local config or the admin listener; add `--url http://127.0.0.1:4098 --token-file /etc/reiki/admin.token` when those defaults differ. The [admin API](monitoring.md) documents the same data as HTTP.

| Symptom | First commands | What the output means | Fix |
| --- | --- | --- | --- |
| Client gets `502` `bad_gateway` | `reiki failures --limit 20`, `reiki keys`, then `grep 'upstream key failed\|falling back\|FORWARDER' /var/log/reiki.log` | A `transient` ring row with `STATUS -` is a connect or timeout; `falling back` means the chain ended; a forwarder failure blocks fallback | Check the upstream address and network, re-enable a key with `reiki keys --enable NAME`, or fix the `[upstream.X.forward]` link |
| Client gets `503` `reiki_exhausted` | `reiki keys`, `reiki failures --limit 20` | `cooling` or `disabled` on every key in the chain; `Retry-After` equals the shortest recovery wait | Wait for the cooldown, set `quota_resets` on the key, rotate the disabled key file, or run `reiki keys --enable NAME` |
| Client gets `504` `gateway_timeout` | `reiki failures --limit 20`, `reiki keys`, then `grep 'not replaying a non-idempotent request' /var/log/reiki.log` | Header or body timed out; a `POST` or `PATCH` stays on its first attempt | Raise the upstream timeout for slow endpoints, or retry at the client after checking whether the provider accepted the write |
| Client gets `429` | `reiki failures --limit 20`, `reiki keys`, `grep 'failed login' /var/log/reiki.log` | `rate_limited` or `reiki_shaped` `error.type` is Reiki's own limit; a `rate_limited` ring row is an upstream 429; `failed login` marks ten bad tokens from one address | Wait for `Retry-After`, raise `rate_limit_per_minute`, reduce local shaping, or correct the client's stream token |
| Client gets `401` | `printf '%s' "$TOKEN" \| reiki hash-token`, `reiki check` | `unauthorized` with `missing token` or `unknown token` comes from the client listener; `invalid credential` in an upstream failure disables one key | Send the stream token in `Authorization: Bearer`, match it to a stream's `token_sha256`, and reload after a change |
| A key is cooling or disabled | `reiki keys --json`, `reiki failures --limit 20 --key NAME` | `state` shows `cooling`, `out_of_rotation` or `available`; `cooldown_class` names the cause; `scopes` holds route-and-model cooldowns | Use the durations and precedence in [response classification](classification.md#cooldown-values), then `reiki keys --enable NAME` when the key is good again |
| Requests are slow | `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:4097/__reiki/healthz`, `reiki keys`, `reiki failures --limit 20`, `reiki top --once` | `ok` confirms the client listener is answering; a growing failure ring shows upstream waits; `reiki top --once` shows fleet cooldowns | Check the upstream's latency, the stream limit and shaping settings, and the 8 MiB request body ceiling |
| The tunnel does not intercept | `reiki check`, `grep 'tunnel refused a destination\|intercepted handshake failed' /var/log/reiki.log`, `reiki failures --limit 20` | `403` means the destination missed `allow`; `421` marks a `Host` mismatch; `407` asks for the stream token; `400` marks an unsafe path; allowed hosts absent from `hosts` stay opaque | Add the host to `[tunnel.intercept]`, map it to an upstream on the same host, allow port 443, send `Proxy-Authorization`, and trust `ca.pem` in the client |
| A peer shows stale | `reiki peers --json`, `reiki peers --watch` | `state: unknown` with `stale data` means the reported age plus time since receipt passed three poll intervals; the default interval is 30 seconds | Restore the peer listener, its token and its clock, or lower `poll_interval_secs` in `[peers]` |

`reiki failures` takes `--limit` from 1 to 64, defaults to 20, filters with `--key NAME`, and prints the same ring as JSON with `--json`. `reiki keys` prints a table by default and adds `scopes` with `--json`. Both exit 0 on a good answer, 1 on a refusal and 2 when the admin listener cannot be reached.

## Worked example: quota, rate limit or request size

Run `reiki failures --limit 20`:

```text
TIME                 STREAM    KEY     STATUS CLASS           COOLED NOTE
2026-11-18 10:04:11  client-a  zen-a   429    quota_exhausted  18000s quota exceeded for this key
2026-11-18 10:04:18  client-b  zen-a   429    rate_limited     60s    rate limit exceeded
2026-11-18 10:05:02  client-a  zen-b   402    quota_exhausted  60s    credit balance is too low
```

- The `quota_exhausted` row with a whole-key cooldown of `18000s` is an out-of-quota answer. The `18000s` is the configured reset or `Retry-After`; the [cooldown table](classification.md#cooldown-values) gives the precedence.
- The `rate_limited` row with `60s` is an upstream rate limit. `reiki keys --json` shows it under `scopes` when the request carried a model, on the key itself otherwise.
- The `402` row is quota exhaustion from a body that says the credit balance is low. A request-size refusal with `fewer max_tokens` or `can only afford` is a pass-through, so it produces no ring row; the client sees the scrubbed `402` body with the phrase. Confirm with `reiki failures --limit 20 --key zen-b --json` and the client's response body.

The `COOLED` column prints seconds for a key that started cooling and `yes` for a disabled key. `STATUS -` marks a transport failure. The ring is in memory, holds the latest 64 entries and starts empty after a restart.

## Check the setup

```bash
reiki check
reiki keys --check
```

`reiki check` parses the config, reads every key file and prints the number of upstreams, keys and streams. It exits 0 when the setup is valid; on failure it prints the problem to stderr and exits 1. `reiki keys --check` checks key files only: each must exist, be readable, be non-empty and keep group and other users from reading it. It exits 0 when all files are clean, 1 for a problem and 2 when the config cannot be read. Neither command prints a key value.

## Logs

Reiki's own log is separate from the [request log](requestlog.md). By default it goes to stderr at `info`; the `[log]` table changes that:

```toml
[log]
level = "info,reiki::tunnel=debug"
file = "/var/log/reiki.log"
format = "text"
stderr = true
file_max_bytes = 52428800
file_keep = 5
```

- `RUST_LOG`, when set, overrides `level` at startup.
- SIGHUP and `POST /reload` apply a changed `level` without a restart and reopen the file. Changing `file`, `format`, `stderr`, `file_max_bytes` or `file_keep` needs a restart; see [reload](#reload).
- The file is created with mode 0640, and an existing file is tightened to 0640. A symbolic link at the path is refused. A line that cannot be written is dropped and counted; the first failure is reported once on stderr.
- Every line made while a request is handled carries its `id` in the span prefix (`req{id=…}`); the same `id` is on that request's records in the request log.
- `info` logs one line per forwarded request with the stream, key name, method, path and upstream status, plus tunnel open and close lines.
- `warn` logs `upstream key failed` with the class, whether the key started cooling and the seconds left, plus refused tunnel destinations and refused intercepted requests.
- `debug` logs the first 160 characters of upstream error bodies and tunnel I/O errors. Keep `debug` off in shared logs: some providers echo part of a rejected key in error bodies.

Key values and stream tokens stay out of the log.

## Reload

Send `SIGHUP` (`kill -HUP <pid>`) or `POST /reload` to the admin listener to apply a changed config without a restart. Both re-read the config, key files and interception CA files, build and check the whole new state first, and swap it in only if every part is valid; a bad config keeps the running one and reports why. Requests and tunnels already open finish on the state they started with.

```bash
reiki reload --dry-run
reiki reload --dry-run | jq .changed
reiki reload --token-file /etc/reiki/admin.token
```

`reiki reload` takes `--dry-run`, `--url` and `--token-file`, falling back to `[admin] listen` and `REIKI_ADMIN_TOKEN`. It prints the JSON answer: exit 0 for success, 1 for a refusal or failed config, and 2 when the instance cannot be reached. The [admin reference](monitoring.md#post-reload) shows the response fields.

To reload without a signal, turn on the watcher:

```toml
[reload]
watch = true
interval_secs = 5
```

Every interval Reiki checks the config file, key files, interception CA files and any file a plugin reads, by size, mtime, inode and ctime. When they changed and then stayed unchanged for one more interval, it reloads. A config that fails to load is tried once and left alone until a file changes again. `watch` and `interval_secs` apply on the next reload.

Applied live: keys, streams, upstreams and the admin token hash; plugin settings (an unchanged `[plugins.<name>]` table keeps its counters and open files); files a plugin reads; the tunnel `allow` list, `deny_private`, `allow_loopback` and `connect_timeout_secs`; the intercept host table, `require_auth` and `leaf_ttl_hours`; and the interception CA. A CA swap checks the key, expiry and name constraint before anything changes, warns clients to trust the new CA, and leaves open connections on their old certificate.

Needs a restart, reported as `restart_needed`: every `listen` address, the `[log]` file settings, and adding or removing `[tunnel]` or `[tunnel.intercept]`.

`GET /status` shows `config.generation`, `config.loaded_at` and `config.last_reload`, with the error when the last attempt failed.

## Key health

`reiki keys` prints live health, last status code and streams for every key from the admin listener; see the [admin reference](monitoring.md#get-status). `reiki keys --enable NAME` clears a key's cooldown, disabled flag, failure count and combination cooldowns, and saves the change.

Key health is saved next to the config as `<config>.state` and restored at startup, so cooldowns and out-of-rotation keys survive a restart. The directory must be writable by the user Reiki runs as; otherwise Reiki logs a warning and runs without persistence. A reload keeps the live health of keys whose files did not change, resets keys whose files did, and leaves the state file unread. The file holds key names, timings and a SHA-256 of each key, plus the learned outbound cap, pause and daily and monthly counters, with no key value.

A key whose digest differs, or whose file cannot be read, counts as rotated and starts healthy; state without a digest falls back to the file's modified time. A missing or unreadable state file is ignored with a warning.

## Health probe and admin routes

`GET /__reiki/healthz` on the client listener returns `200` with the body `ok` while Reiki is up. The probe needs no token and answers from the client listener even when upstreams are cooling.

The admin listener serves `/status`, `/failures`, `/reload` and `/keys/enable` under both bare paths and `/v1`, plus `/v1/openapi.json` and any plugin route such as `/metrics` and `/usage`. Missing or wrong tokens get a bare `401`; ten bad tokens from one address in a minute get a `429` with `Retry-After`. See [monitoring](monitoring.md) for the full reference.

Classification rules and cooldown numbers live in one place: [response classification](classification.md).
