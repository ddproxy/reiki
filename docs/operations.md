# Operations

This page is for the operator of a running Reiki instance: upgrade, rollback, backup, restore, logs, metrics and health checks. For the first install see [install](install.md); for config details see [configuration](configuration.md).

## Upgrade

1. Read the target section of the [changelog](../CHANGELOG.md) and the [upgrade notes](upgrades.md). Note new config keys, changed defaults and any `Requires:` action.
2. Install the new binary without replacing the running one. Check it before switching:

   ```sh
   reiki --config /etc/reiki/reiki.toml check
   reiki --config /etc/reiki/reiki.toml keys --check
   reiki --config /etc/reiki/reiki.toml reload --dry-run
   ```

3. If `restart_needed` is empty, apply the reload with `reiki --config /etc/reiki/reiki.toml reload`. Otherwise stop with `SIGTERM`, swap the binary, and start again. Any change that adds, removes or edits `[stream_admin]`, the listener addresses or TLS needs a restart.
4. Confirm the proof: `curl -s http://127.0.0.1:4097/__reiki/healthz` answers `ok`, and `GET /v1/status` on the admin listener answers with the expected counts. See [monitoring](monitoring.md#get-v1status).

Upgrade one instance at a time across a fleet. Peers use strict wire shapes, so a mixed fleet may show `unknown` rather than a partial view.

## Rollback

Keep the previous binary and config beside the current ones, for example `/usr/local/bin/reiki.previous` and `/etc/reiki/reiki.toml.previous`. On a failed upgrade: stop the instance, restore the previous binary (mode `0755`) and config, and start it again. State files written by the newer binary may not be readable by an older one; restore `<config>.state` from the backup taken before the upgrade if the older binary refuses it. Do not roll back a config that still contains keys introduced by the new release. See [upgrades](upgrades.md#single-machine-upgrade).

## Backup and restore

Back up config and files with private modes; never paste their contents into a ticket.

| File | Default path | Mode | Backup rule |
| --- | --- | --- | --- |
| Config | `/etc/reiki/reiki.toml` | `0640` | Copy before a config change; it contains token hashes, not raw tokens. |
| Key files | `/etc/reiki/keys/NAME` | `0600` | Copy with an existing backup system; a rotated key changes the saved digest in the state file. |
| Key health and shaping state | `/etc/reiki/reiki.toml.state` | `0600` | Copy while stopped, or immediately after a successful write; an unknown shape is ignored and rebuilt. |
| Dynamic stream state | `/etc/reiki/stream-admin.json` (or `stream_admin.state_file`) | `0600` | Copy while stopped; holds token hashes and ids, never token values. |
| Stream audit log | `/etc/reiki/stream-admin-audit.jsonl` (or `stream_admin.audit_log`) | `0600` | Append-only; copy the whole file plus its `.head` sidecar together. Never truncate or edit either. |
| Request log | `[plugins.requestlog] file`, e.g. `/var/lib/reiki/requests.jsonl` | `0600` | Optional; summaries by default, bodies only with `capture_bodies = true`. |

Restore with the instance stopped: put files back with the owner and mode from the table, then start and check `GET /v1/status` (including `stream_admin.state_error`) and `reiki streams audit verify FILE` on a copy of the audit log. A fresh audit file starts a new chain at record 1, so keep old JSONL and sidecar pairs for earlier periods. See [dynamic streams](dynamic-streams.md#backup-and-restore) and [upgrades](upgrades.md#dynamic-stream-state).

## Logs and metrics

- Service log: stderr and `[log] file`; formats `text`, `json` and `ocsf`; level from `[log].level`. `log.write_errors` in `/v1/status` counts dropped file writes.
- Request log: `GET /v1/requestlog/metrics` when `[plugins.requestlog]` is configured; summaries only unless `capture_bodies = true`. See [request log](requestlog.md).
- Metrics: `GET /v1/metrics` on the admin listener. Series include key and stream request counts, gateway rejections and store errors, and stream-admin counters when configured. Labels are bounded and carry no token, key or model value. See [monitoring](monitoring.md#get-v1metrics).
- Audit: no HTTP route. Verify offline with `reiki streams audit verify FILE`. See [dynamic streams](dynamic-streams.md#verify-the-audit-log).
- Dashboards and `reiki top`: give them the optional read-only token (`[admin] read_token_sha256`, sent as `REIKI_ADMIN_READ_TOKEN` for `top`). It opens the read routes and answers `403` (`read_only_token`) on reload and key enable. Reload and key-enable audit events carry the caller's `X-Reiki-Actor` label when one is sent. See [monitoring](monitoring.md#read-only-token).

## Health checks

```sh
curl -fsS http://127.0.0.1:4097/__reiki/healthz
REIKI_ADMIN_TOKEN="$(cat /etc/reiki/admin.token)" reiki keys
REIKI_ADMIN_TOKEN="$(cat /etc/reiki/admin.token)" reiki failures --limit 20
```

The health probe needs no token and answers `ok` while the listener is up; it does not prove an upstream works. `reiki keys` shows `ok`, `cooling` or `disabled` per key and exits `0` on a good answer, `1` on a refusal and `2` when the admin listener cannot be reached. `reiki top --once` adds a fleet view when peers are configured. See [monitoring](monitoring.md) and [top](top.md).

`reiki_gateway_store_ok` on `/v1/metrics` is `0` while the gateway store is closed. A store closed by a failed state write retries a probe write of the current counters every 5 to 30 seconds and reopens after disk space or permissions return, without a restart or new traffic. A store closed by an unreadable state file at startup is not probed by the timer; see [Troubleshoot gateway store unavailable](#troubleshoot-gateway-store-unavailable).

## Startup checks

Before binding listeners, Reiki probes the state directory (the parent of `<config>.state`) and, when `[log] file` is configured, the log directory. Each probe creates and removes a uniquely named temp file in that directory and writes a few KB to exercise write permission and free space. The probe does not create files outside the configured paths and does not leave a probe file behind.

With `[gateway] on_store_error = "closed"` (the default), a state directory probe failure exits with a message naming the path and the OS error. With `"open"`, Reiki logs one `WARN` and continues. A log directory probe failure never stops startup in either mode: Reiki logs one `WARN` and disables the log file for that run.

## Troubleshoot gateway store unavailable

A client gets `503` with `{"ok":false,"error":{"code":"gateway_store_unavailable","message":"per-stream counters are unavailable"}}` when the gateway store is closed and the stream is fail-closed (`[gateway] on_store_error = "closed"`, the default, or a per-stream override). With `"open"`, requests continue while the failure is counted in `reiki_gateway_store_errors_total{stream}` and reported as `GatewayStoreError` events. See [gateway admission](shaping.md#gateway-admission) for the two modes.

The store closes when a state write fails, for example `ENOSPC`, a read-only state file or directory, or another I/O error. It also closes at startup when an existing `<config>.state` cannot be read or parsed, including a corrupt file. A missing state file is not an error: counters and saved cooldowns start fresh. Check free space, mounts, and the owner and mode of the state directory and file (mode `0600`; see [backup and restore](#backup-and-restore)).

For a failed write, the background `reiki-state` writer probes the current counters on a timer without waiting for new traffic: 5 seconds after the failure, again after 5, then 10, 20 and a 30-second cap while writes keep failing. The first successful write reopens admission. The transition logs one `WARN` on close and one `INFO` on recovery, not one line per refused request. `reiki_gateway_store_ok` is `0` while closed and `1` while open; see the [alert rules](monitoring.md#alert-rules).

A store closed at startup because the state file was unreadable or corrupt is different. The timer deliberately does not probe it, because writing would replace the file with zeroed counters. Any later successful state write, such as one triggered by traffic, would still reopen the store and overwrite the file, so do not wait for it: restore or repair `<config>.state`, fix its owner and mode, and restart Reiki. Removing a damaged file also works if losing saved counters and cooldowns is acceptable.

Check the journal for the transition and the underlying cause:

```sh
journalctl -u reiki --since '1 hour ago' | grep -E 'gateway counter store|cannot write key state|cannot read key state|ignoring key state'
journalctl -u reiki -f
```

`gateway counter store unavailable` is the `WARN` on close and `gateway counter store recovered` the `INFO` on recovery. `cannot write key state` carries the path and OS cause of a failed write. At startup, `cannot read key state; starting fresh` and `ignoring key state in an unsupported format; starting fresh` carry the path and the read or parse cause. All are visible at the default `info` level; see [logs and metrics](#logs-and-metrics).

## Common failures

| Exact message or status | Meaning | Fix |
| --- | --- | --- |
| `unauthorized` / `unknown token` | The client token does not match a stream hash. | Send `Authorization: Bearer <stream token>` and reload after changing `token_sha256`. |
| `503 gateway_store_unavailable` | Counter state is unavailable and `[gateway] on_store_error = "closed"`. | Check the state directory and `<config>.state`; `"open"` is an explicit fail-open choice. See [troubleshooting](#troubleshoot-gateway-store-unavailable). |
| `429 gateway_usage_limited` | `/v1/usage` exceeded `[gateway] usage_rpm` (default 60). | Wait for `Retry-After` or raise the budget. |
| `503 a fail-closed plugin failed; the request was not processed` | A masking or blocking plugin skipped closed. | Read the service log, fix the plugin; `on_error = "pass_through"` opts out deliberately. |
| `state directory ... is not writable` | The startup probe could not create, write and remove a temp file in the parent of `<config>.state`. With `[gateway] on_store_error = "closed"` (default) startup fails; with `"open"` Reiki logs a `WARN` and continues. | Fix owner, mode or free space, or add the directory to `ReadWritePaths=`. |
| `log directory ... is not writable` | The startup probe could not create, write and remove a temp file in the directory of `[log] file`. With `[gateway] on_store_error = "closed"` (default) startup fails; with `"open"` Reiki logs a `WARN` and continues without the log file. | Fix owner, mode or free space, or remove `[log] file`. |
| `admin listen must differ from the client listen address` | Two listeners share an address. | Give each listener its own loopback address and port. |
| `state directory ... is not writable` after a reload | Reload cannot persist the new state. | Restore write access before reloading again. |
| `audit line N: broken chain` | The JSONL file or the `.head` sidecar was edited, truncated or lost. | Restore both from the last good backup; a fresh file starts a new chain. |

The full symptom-to-fix table is [troubleshooting](troubleshooting.md). Exit codes for every command are in [CLI](cli.md#exit-codes) and [AGENTS.md](../AGENTS.md).
