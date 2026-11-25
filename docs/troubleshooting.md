# Troubleshooting

Start with the symptom. Every command below reads local config or the admin listener; add `--url http://127.0.0.1:4098 --token-file /etc/reiki/admin.token` when those defaults differ. The [admin API](monitoring.md) documents the same data as HTTP.

| Symptom | First commands | What the output means | Fix |
| --- | --- | --- | --- |
| Client gets `502` `bad_gateway` | `reiki failures --limit 20`, `reiki keys`, then `grep 'upstream key failed\|falling back\|FORWARDER' /var/log/reiki.log` | A `transient` ring row with `STATUS -` is a connect or timeout; `falling back` means the chain ended; a forwarder failure blocks fallback | Check the upstream address and network, re-enable a key with `reiki keys --enable NAME`, or fix the `[upstream.X.forward]` link |
| Client gets `503` `reiki_exhausted` | `reiki keys`, `reiki failures --limit 20` | `cooling` or `disabled` on every key in the chain; `Retry-After` equals the shortest recovery wait | Wait for the cooldown, set `quota_resets` on the key, rotate the disabled key file, or run `reiki keys --enable NAME` |
| Client gets `504` `gateway_timeout` | `reiki failures --limit 20`, `reiki keys`, then `grep 'not replaying a non-idempotent request' /var/log/reiki.log` | Header or body timed out; a `POST` or `PATCH` stays on its first attempt | Raise the upstream timeout for slow endpoints, or retry at the client after checking whether the provider accepted the write |
| Client gets `429` | `reiki failures --limit 20`, `reiki keys`, `grep 'failed login' /var/log/reiki.log` | `gateway_*`, `rate_limited` (login backoff) or `reiki_shaped` `error.code` is Reiki's own limit; a `rate_limited` ring row is an upstream 429 | Wait for `Retry-After`, raise the relevant `[streams.NAME.limits]` value, reduce local shaping, or correct the client's stream token |
| Client gets `401` | `printf '%s' "$TOKEN" \| reiki hash-token`, `reiki check` | `unauthorized` with `missing token` or `unknown token` comes from the client listener; `invalid credential` in an upstream failure disables one key | Send the stream token in `Authorization: Bearer`, match it to a stream's `token_sha256`, and reload after a change |
| A key is cooling or disabled | `reiki keys --json`, `reiki failures --limit 20 --key NAME` | `state` shows `cooling`, `out_of_rotation` or `available`; `cooldown_class` names the cause; `scopes` holds route-and-model cooldowns | Use the [cooldown rules](failover.md), then `reiki keys --enable NAME` when the key is good again |
| Requests are slow | `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:4097/__reiki/healthz`, `reiki keys`, `reiki failures --limit 20`, `reiki top --once` | `ok` confirms the client listener is answering; a growing failure ring shows upstream waits; `reiki top --once` shows fleet cooldowns | Check upstream latency, stream limits and shaping, and the 8 MiB request body ceiling |
| The tunnel does not intercept | `reiki check`, `grep 'tunnel refused a destination\|intercepted handshake failed' /var/log/reiki.log`, `reiki failures --limit 20` | `403` means the destination missed `allow`; `421` marks a `Host` mismatch; `407` asks for the stream token; `429` means too many bad `Proxy-Authorization` attempts; `400` marks an unsafe path | See [diagnose tunnel denials](#diagnose-tunnel-denials) and [tunnel](tunnel.md) |
| A peer shows stale | `reiki peers --json`, `reiki peers --watch` | `state: unknown` with `stale data` means the reported age plus time since receipt passed three poll intervals; the default interval is 30 seconds | Restore the peer listener, its token and its clock, or lower `poll_interval_secs` in `[peers]` |
| Client gets `503` `a fail-closed plugin failed; the request was not processed` | `grep 'plugin.*panic\|plugin.*failed' /var/log/reiki.log`, `reiki check` | A fail-closed plugin panicked in `new_state` or a response hook while `on_error` is `reject`; buffered responses are refused and streamed bodies stop at 503 | Fix the plugin. An explicit `on_error = "pass_through"` opts out, but lets unprocessed or unmasked traffic through |
| Reiki refuses to start or reload | `journalctl -u reiki -f`, `reiki keys --check` | A key file has group or other read/write bits, an error with the default `strict_key_permissions = true` | `chmod 0600` the key file, or set `strict_key_permissions = false` to accept it with a warning |
| Admin listener will not start | `journalctl -u reiki -f`, `reiki check` | `[admin].listen` is not a literal loopback IP and port; hostnames such as `localhost:9090` are refused | Use `127.0.0.1:9090` or `[::1]:9090`, with TLS terminated in front for remote access |
| Reiki exits before listening | `journalctl -u reiki -f` | `state directory ... is not writable` names the path and OS cause; a `log directory ... is not writable` line is only a warning and the log file is off for that run | With `[gateway] on_store_error = "closed"` (default) fix owner, mode or free space, or add the directory to `ReadWritePaths=`; with `"open"` Reiki logs a `WARN` and continues |
| Gateway counters are unavailable | `curl -s -H "Authorization: Bearer $ADMIN_TOKEN" http://127.0.0.1:4098/v1/metrics \| grep reiki_gateway_store_ok`, `journalctl -u reiki -f` | `reiki_gateway_store_ok` is `0`; the store closed on a failed write or an unreadable state file | Follow [troubleshoot gateway store unavailable](deploy.md#troubleshoot-gateway-store-unavailable) |

`reiki failures` takes `--limit` from 1 to 64, defaults to 20, filters with `--key NAME` and `--stream NAME`, and prints the same ring as JSON with `--json`. The `ACTION` column says what the failover walk did: `cooldown`, `rotate` or `pass-through`. `reiki keys` prints a table by default and adds `scopes` with `--json`. Both exit 0 on a good answer, 1 on a refusal and 2 when the admin listener cannot be reached. Exit codes for every command are in [CLI](cli.md).

## Worked example: quota, rate limit or request size

Run `reiki failures --limit 20`:

```text
TIME                 STREAM    KEY     STATUS CLASS           ACTION      COOLED NOTE
<time>               client-a  zen-a   429    quota_exhausted  cooldown    18000s quota exceeded for this key
<time>               client-b  zen-a   429    rate_limited     cooldown    60s    rate limit exceeded
<time>               client-a  zen-b   402    quota_exhausted  cooldown    60s    credit balance is too low
```

- The `quota_exhausted` row with a whole-key cooldown of `18000s` is an out-of-quota answer. The `18000s` is the configured reset or `Retry-After`; the [cooldown rules](failover.md) give the precedence.
- The `rate_limited` row with `60s` is an upstream rate limit. `reiki keys --json` shows it under `scopes` when the request named a model, on the key otherwise.
- The `402` row is quota exhaustion from a body that says the credit balance is low. A request-size refusal with `fewer max_tokens` or `can only afford` is a `request_error` row with `ACTION pass-through`; the key is not cooled and the client sees the scrubbed `402` body. Confirm with `reiki failures --limit 20 --key zen-b --json` and the client's response body.

`ACTION` is `cooldown` when the key or one scope cooled, `rotate` when an `unauthorized` answer disabled the key until its file is replaced, and `pass-through` when the key was only skipped for that request. A `pass-through` row still has `COOLED -`. The `COOLED` column prints seconds for a key that started cooling and `yes` for a disabled key; `STATUS -` marks a transport failure. The ring holds `failure_ring_size` records in memory (`200` by default, `0` disables) and starts empty after a restart. `reiki_failure_ring_entries` and `reiki_failure_ring_dropped_total` on `/v1/metrics` show its current size and drops. `note` is a printable body snippet of at most 200 characters with credentials scrubbed; the full row shape is in [`GET /v1/failures`](monitoring.md#get-v1failures).

## A 429 with healthy keys

`reiki keys` shows key health, not client limits. Four gates answer `429`:

| Where | What it means | What it does to keys |
| --- | --- | --- |
| Gateway admission | A `[streams.NAME.limits]` budget or fair-share turn was reached; `error.code` starts with `gateway_`, with `Retry-After` at least 1 | None |
| Local shaping | An upstream `limits` budget held the request; `reiki_shaped` with `x-reiki-origin: local` | None |
| Login backoff | Too many wrong stream or admin tokens from one address | None |
| Upstream | A `rate_limited` or `quota_exhausted` ring row | Cools a scope or the key |

Run `reiki failures --limit 20` and `reiki keys`. A `rate_limited` ring row with a `60s` cooldown is an upstream answer; a `429` with no ring row is Reiki's own gate. With the default `[auth]` settings, ten wrong tokens from one address in a minute make the next attempt get `429`; `grep 'failed login'` confirms it. See [failover](failover.md) and [shaping](shaping.md).

## Credentials and bypass

The upstream key leaves the machine: Reiki reads it from a file only its user can read and sets the credential header on each upstream request to the configured `base_url` over HTTPS (plain HTTP is allowed only on loopback) or to a configured forwarder socket or command. It is not written to config, client answers, request-log records or the state file. An upstream echo is scrubbed; see [security](security.md). The `debug` service log carries the first 160 characters of upstream error bodies, which can include part of a rejected key, so keep it off in shared logs. The stream token is separate: the client sends it to Reiki and only its SHA-256 hash is stored.

Two deliberate exceptions: a `passthrough` upstream forwards the client's own credentials unchanged, and an opaque tunnel copies the client's bytes without reading them. Only hosts listed under `[tunnel.intercept]` are read; an allowed host that is not listed stays opaque and uses the client's own credentials. Reiki sees only traffic pointed at it, so the SDK must use Reiki as its base URL, for example `http://127.0.0.1:4097/<upstream>` with the stream token as its API key. See [clients](clients.md), [providers](providers.md) and [tunnel](tunnel.md).

## When keys run out

With no fallback, the client gets `503` `reiki_exhausted`. `Retry-After` is the shortest wait until a non-disabled key recovers, rounded up to at least one second; when every usable key is disabled there is no `Retry-After`. If a key refused the request for a reason that does not cool it, such as `access_disabled`, the client sees that upstream's last status and body. If every key was shaped out, the answer is `429` `reiki_shaped`. A configured fallback upstream is tried once before any `503`.

Whole-key cooldowns outlive a restart through `<config>.state`; route-and-model cooldowns live in memory. A key file whose digest changed starts healthy. See [key health](#key-health) and [failover](failover.md).

## Key state details

A `401`, a `400` whose parsed JSON has a real `"reason": "API_KEY_INVALID"` property, or a `403` naming an invalid, expired or revoked credential disables the key. Other `403` answers are `access_disabled`: the key is skipped for that request only. One request disables at most one key; a second `401` in the same walk is tried but left alone. With `hold_unauthorized = true`, the disable waits until a later key serves the request; if none does, no key is disabled. A `429` cools but does not disable.

A `rate_limited` failure and a 402 treated as quota exhaustion are scoped to `(route, model)` when the request names a model; a request with no model cools the whole key. A quota `429` is always key-wide. When enough distinct models are cooling, `scope_escalate_after` (default 3) escalates the next combination failure to a whole-key cooldown. Re-enabling a key with `reiki keys --enable NAME` clears its cooldown, disabled flag, failure count and scoped cooldowns. See [failover](failover.md) and [keys](keys.md).

## Failover cost

| Cost | Where it shows |
| --- | --- |
| One extra upstream attempt per skipped key | `attempts` in the request log; `reiki_failovers_total` with the metrics plugin |
| One gateway admission for the request, one outbound budget per attempt | Stream and upstream limits |
| A cooldown shared by every stream that lists the key | `reiki keys`, `/v1/status`, `reiki top` |
| A 3-second wait and one repeated request when `confirm_quota = true` | First quota `429` only; it still enters the failure ring |

There is no replay after a `2xx` or `3xx` status line: a body that breaks mid-stream stays broken. Transport failures replay only when a side effect cannot be duplicated; an explicit upstream error response fails over for every method. See [failover](failover.md).

## POST after a timeout

A `POST` or `PATCH` whose bytes may have reached the upstream is not replayed on another key: a retry could duplicate a side effect. Reiki returns `504` on timeout or `502` on connection failure. Idempotent methods (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`, `TRACE`) and a failure before any bytes were sent do move to the next key. An explicit upstream error status is not a transport failure and fails over for every method. See [failover](failover.md).

## Key rotation

Write the new secret to a mode `0600` file, add its `[keys.NAME]` table and put it in the chain in the same config edit:

```bash
umask 077
printf '%s' "$NEW_PROVIDER_KEY" > /etc/reiki/keys/zen-b
chmod 0600 /etc/reiki/keys/zen-b
```

```toml
[keys.zen-b]
upstream = "zen"
file = "/etc/reiki/keys/zen-b"

[streams.client-a]
token_sha256 = "<unchanged 64 hex characters>"
chain = ["zen-b"]
strategy = "sticky"
```

```bash
reiki --config /etc/reiki/reiki.toml check
reiki --config /etc/reiki/reiki.toml keys --check
reiki --config /etc/reiki/reiki.toml reload --dry-run --token-file /etc/reiki/admin.token
reiki --config /etc/reiki/reiki.toml reload --token-file /etc/reiki/admin.token
reiki --config /etc/reiki/reiki.toml keys --token-file /etc/reiki/admin.token
```

A successful reload swaps the whole config. New requests use only the new chain; requests in flight keep the old key until they finish. A changed file digest makes the key start healthy, and the state file holds the digest, not the secret. Revoke the old key at the provider only after in-flight requests drain; Reiki does not revoke it. See [rotate a key without downtime](keys.md).

## Scan and mask before blocking

`reiki scan` reads stdin or files and prints `file:line:col plugin kind` for each finding, one JSON object per finding with `--json`; exit `1` means findings and `0` means clean. It does not change traffic.

```toml
[plugins.phi]
mode = "block"
```

In `block` mode a request body with a finding is rejected before a key is used, and the refusal names kinds and counts, never values. Only the request body is checked; in `block` mode an encoded or invalid UTF-8 body gets `415`, while `report` mode logs it as unchecked.

```toml
[plugins.placeholders]
terms = ["<term>"]
```

Masking replaces a configured term and credential-shaped strings before the request leaves Reiki: the client sends the original text, the upstream sees `[[REIKI_TERM_<id>]]` and `[[REIKI_SECRET_<id>]]` placeholders, and the client receives the original text byte for byte. The request log holds masked request and response bodies. A private key becomes `[[REIKI_PRIVATE_KEY_REMOVED]]` and is not restored. Headers, paths and query strings are not masked; images, PDFs and thinking payloads are not scanned. See [plugins](plugins.md).

## Diagnose tunnel denials

With an allow-list of `api.example.com` and an intercepted host that requires a stream token:

```toml
[tunnel]
listen = "127.0.0.1:4099"
allow = ["api.example.com"]

[tunnel.intercept]
ca_cert = "/etc/reiki/ca/ca.pem"
ca_key = "/etc/reiki/ca/ca.key"
require_auth = true

[[tunnel.intercept.hosts]]
host = "api.example.com"
upstream = "example"
stream = "client-a"
```

| Cause | Client sees |
| --- | --- |
| Destination not allowed | `403`, body `destination not allowed` |
| Path outside the intercepted API | `403` with `error.code` `forbidden` |
| `Host` header differs from the tunnel destination | `421` with `error.code` `misdirected` |
| Intercepted host without a valid stream token | `407`, `proxy-authenticate: Basic realm="reiki"`, body `a stream token is required` |
| Too many bad `Proxy-Authorization` attempts from one address | `429`, `retry-after: <seconds>`, body `too many failed logins` |

A valid bearer token, or a basic password equal to the stream token, gets `200 Connection Established` and clears that address's failed-login count. A plain CONNECT to an allowed host needs no token: the allow-list is the gate. Opaque tunnels have no request log, placeholders, key injection or failover. See [tunnel](tunnel.md).

## Check the setup

```bash
reiki check
reiki keys --check
```

`reiki check` parses the config, reads every key file and prints counts of upstreams, keys and streams. It exits 0 when the setup is valid; on failure it prints the problem to stderr and exits 1. `reiki keys --check` checks key files only: each must exist, be readable, be non-empty and keep group and other users from reading it. It exits 0 when all files are clean, 1 for a problem and 2 when the config cannot be read. Neither command prints a key value.

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
- SIGHUP and `POST /v1/reload` apply a changed `level` without a restart and reopen the file. Changing `file`, `format`, `stderr`, `file_max_bytes` or `file_keep` needs a restart; see [reload](#reload).
- The file is created with mode `0640`, and an existing file is tightened to `0640`. A symbolic link at the path is refused. A line that cannot be written is dropped and counted; stderr warns at most once a minute, and the count is on `GET /v1/status` as `log.write_errors`.
- Every line made while a request is handled carries its `id` in the span prefix (`req{id=…}`), the same `id` as that request's records in the request log.
- `info` logs one line per forwarded request; `warn` logs `upstream key failed` with class and cooldown, refused tunnel destinations and refused intercepted requests; `debug` logs the first 160 characters of upstream error bodies and tunnel I/O errors.

Key values and stream tokens stay out of the log.

## Reload

Send `SIGHUP` (`kill -HUP <pid>`) or `POST /v1/reload` to apply a changed config without a restart. Both re-read the config, key files and interception CA files, build and check the whole new state first, and swap it in only if every part is valid; a bad config keeps the running one. Requests and tunnels already open finish on the state they started with.

```bash
reiki reload --dry-run
reiki reload --dry-run | jq .changed
reiki reload --token-file /etc/reiki/admin.token
```

`reiki reload` takes `--dry-run`, `--url` and `--token-file`, falling back to `[admin] listen` and `REIKI_ADMIN_TOKEN`. It exits 0 for success, 1 for a refusal or failed config and 2 when the instance cannot be reached. The [admin reference](monitoring.md#post-v1reload) shows the response fields and the [reload map](deploy.md#reload-vs-restart) lists which settings apply live.

To reload without a signal, turn on the watcher:

```toml
[reload]
watch = true
interval_secs = 5
```

Every interval Reiki checks the config file, key files, interception CA files and any file a plugin reads, by size, mtime, inode and ctime. When they changed and then stayed unchanged for one more interval, it reloads. A config that fails to load is tried once and left alone until a file changes again. `watch` and `interval_secs` apply on the next reload. `GET /v1/status` shows `config.generation`, `config.loaded_at` and `config.last_reload`, with the error when the last attempt failed.

## Preview a reload

Change a plugin table, then preview before applying it:

```console
$ reiki --config /etc/reiki/reiki.toml reload --dry-run --token-file /etc/reiki/admin.token
{"dry_run":true,"generation":<n>,"keys":<n>,"streams":<n>,"changed":["plugins phi,requestlog"],"restart_needed":[]}
```

A preview applies nothing: `/v1/status` still reports the old generation and `last_reload: null`. `changed` carries plugin, peer TLS and tunnel or intercept diff labels; key, stream and upstream edits are applied but not named. `restart_needed` lists settings the file changed but the running instance cannot apply. A rejected config leaves the running state intact.

## Key health

`reiki keys` prints live health, last status code and streams for every key from the admin listener; see [`GET /v1/status`](monitoring.md#get-v1status). `reiki keys --enable NAME` clears a key's cooldown, disabled flag, failure count and combination cooldowns, and saves the change.

Key health is saved next to the config as `<config>.state` and restored at startup, so cooldowns and out-of-rotation keys survive a restart. The directory must be writable by Reiki's user: at startup Reiki probes it and, when `[log] file` is configured, the log directory by creating, writing a few KB to and removing an unpredictable temp file (`create_new` plus `O_NOFOLLOW`). A pre-existing probe file or symlink cannot satisfy the check. With `[gateway] on_store_error = "closed"` (default) a failed probe exits 1 naming the path and OS cause; with `"open"` Reiki logs a `WARN` and continues, and a log directory failure disables the log file for that run. A reload keeps the live health of keys whose files did not change, resets keys whose files did, and leaves the state file unread. The file holds key names, timings and a SHA-256 of each key, plus the learned outbound cap, pause and daily and monthly counters, with no key value.

A key whose digest differs, or whose file cannot be read, counts as rotated and starts healthy; state without a digest falls back to the file's modified time. A missing or unreadable state file is ignored with a warning. See [state file](deploy.md#state-file).

## Read-only access and audit labels

Give dashboards and `reiki top` the optional `[admin] read_token_sha256`: it opens read routes and gets `403` with `error.code` `read_only_token` on reload or key enable. `reiki top` reads `REIKI_ADMIN_READ_TOKEN` and falls back to `REIKI_ADMIN_TOKEN`. Reload and key-enable audit events carry the caller's `X-Reiki-Actor` label when one is sent. Refusals use the shared [error envelope](monitoring.md#error-envelope).

Management commands read their raw token from `REIKI_STREAM_ADMIN_TOKEN` or `--token-file`, never from an argument. Mutating commands take `--reason` and an optional `--idempotency-key`; `create` and `token add` take `--ttl`, and `update` takes `--rpm` and `--tpm`. Audit records are verified offline with `reiki streams audit verify FILE` and inspected with `reiki streams audit head FILE --n 5`. See [dynamic streams](dynamic-streams.md) and [monitoring](monitoring.md#stream-administration-routes).

## Health probe and admin routes

`GET /__reiki/healthz` on the client listener returns `200` with the body `ok` while Reiki is up. The probe needs no token and answers even when upstreams are cooling; it does not prove an upstream works.

The admin listener serves `/v1/status`, `/v1/failures`, `/v1/reload`, `/v1/keys/enable`, `/v1/openapi.json` and plugin routes such as `/v1/metrics` and `/v1/usage/streams`. The client listener serves `GET /v1/usage` for a stream's own token; it is not an admin route. `[gateway] usage_rpm` (default 60 per minute) budgets that self-service route. Every listener answers refusals with the shared JSON error envelope, and ten bad tokens from one address in a minute get `429` with `Retry-After`. See [monitoring](monitoring.md) for the full reference.
