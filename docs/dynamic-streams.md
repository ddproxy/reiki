# Dynamic streams

`[stream_admin]` is opt-in dynamic stream management on the admin listener. Without the table there are no management routes, no dynamic streams and no state or audit file access. The operator configures the assignable keys, ceilings and audit file first; the API only assigns what it was allowed to assign. See [configuration](configuration.md#stream_admin) for every key and default, [keys](keys.md) for the credential model and [monitoring](monitoring.md#stream-administration-routes) for the route reference.

This page covers the operating cycle: create, rotate, revoke, expire, limit, audit and back up. Command syntax is also in [CLI](cli.md).

## Before you start

1. Configure `[admin]` with a loopback `listen` and `[stream_admin]` with a dedicated `token_sha256`, an `actor_id_sha256`, `audit_log`, `max_streams`, `allowed_keys`, `ceilings` and `allowed_listen`. The configured `admin.listen` must appear literally in `allowed_listen`; hostnames are refused.
2. Restart the instance. `[stream_admin]` is startup-only: adding, removing or editing the table needs a restart, and a reload preview reports `restart_needed`.
3. Store the raw management token in an owner-only file and export it to the CLI. The raw token is never a command-line argument:

   ```sh
   umask 077
   export REIKI_STREAM_ADMIN_TOKEN="$(cat /etc/reiki/stream-admin.token)"
   ```

   If `require_two_party` is configured, set the second raw token in `REIKI_STREAM_ADMIN_CONFIRM_TOKEN` as well; the CLI requires it even for `list` and `show`. Use `--token-file PATH` instead of the environment variable when a secret manager writes the token to a file.

Defaults worth knowing: `token_env = "REIKI_STREAM_ADMIN_TOKEN"`, `confirm_env = "REIKI_STREAM_ADMIN_CONFIRM_TOKEN"`, `state_file = "stream-admin.json"`, `max_tokens_per_stream = 3`, `default_ttl = 3600`, `max_ttl = 86400`, `max_body_bytes = 16384`, `lockout_threshold = 5`, `lockout_secs = 900` and `rpm = 60`. A mutation fails if its body exceeds `max_body_bytes`, if it exceeds the configured `ceilings` or `max_streams`, or if the audit record cannot be written.

## Create a stream

Each stream name must be unique and must not collide with a static `[streams.NAME]`. The key must exist in `[keys.*]` and be marked `assignable = true` under `[stream_admin.allowed_keys]`; the API never creates provider keys.

```sh
reiki --config /etc/reiki/reiki.toml streams --reason "tenant onboarding" create tenant-a zen-a --ttl 3600
```

The command prints the raw client token **once**. Copy it into the client's secret store now: config, state and audit records hold hashes only, and no route or later command returns the value. If it is lost, revoke its id and issue a new one. `list` and `show` show names, ids and expiries, never token values:

```sh
reiki --config /etc/reiki/reiki.toml streams list
reiki --config /etc/reiki/reiki.toml streams show tenant-a
```

Every mutation needs `--reason` and sends an idempotency key. `--idempotency-key` is generated as `cli-{pid}-{nanos}` when omitted; supply one when retrying a request whose result was lost, so a replay does not create a second stream or token. A reused key with a different request is refused with `409`:

```sh
reiki --config /etc/reiki/reiki.toml streams --reason "tenant limit change" \
  --idempotency-key tenant-a-limit-1 update tenant-a --key zen-b --rpm 90 --tpm 100000
```

With two-party confirmation, the second operator's token in `confirm_env` must accompany every mutation. Failed attempts are throttled per source and locked out after `lockout_threshold` failures for `lockout_secs` seconds.

## Rotate and revoke tokens

Rotation is additive: issue a second token, update the client, then revoke the old id. A stream can hold at most `max_tokens_per_stream` tokens including the first, so revoke before adding when it is full.

```sh
reiki --config /etc/reiki/reiki.toml streams --reason "scheduled rotation" token add tenant-a --ttl 3600
reiki --config /etc/reiki/reiki.toml streams show tenant-a
reiki --config /etc/reiki/reiki.toml streams --reason "scheduled rotation" token revoke tenant-a TOKEN_ID_FROM_SHOW
```

Use the id printed by `show`; it is not a secret. To change the assigned provider key, run `streams update tenant-a --key zen-b` with a `--reason`; the key must already be configured and assignable. To stop a stream without deleting it, revoke every token:

```sh
reiki --config /etc/reiki/reiki.toml streams --reason "tenant offboarded" revoke tenant-a
```

`revoke` keeps the stream, its key assignment and its limits; issue a new token with `token add` when access must resume. The admin API also provides `DELETE /v1/streams/{name}` to remove a dynamic stream entirely; there is no CLI delete command. Static streams cannot be changed through this API.

## Expiry

Every token has an expiry when a TTL is set. `create` and `token add` accept `--ttl SECS`; without it they use `default_ttl` (3600 seconds by default), and a larger value is refused above `max_ttl` (86400 seconds by default). `show` lists token ids with their expiry instants. An expired token is rejected on the client listener the same way an unknown token is; it does not cool the assigned provider key.

Tokens are stored as hashes with ids and expiry times in the state file. Replacing a config stream token resets that stream's gateway counters; dynamic token rotation changes which hash authenticates, not the provider key itself. Static streams have no built-in token expiry; dynamic streams are the way to expire a client credential.

## Limits

Dynamic streams use the same admission path as static streams. Every value sent by the API must stay within `[stream_admin.ceilings]` (`rpm`, `tpm`, `concurrent`, `daily_requests`, `daily_tokens`, `monthly_requests`, `monthly_tokens`). CLI-created streams start without per-stream limits. `update NAME --rpm N --tpm N` sets those two limits, while the HTTP create route can send a fuller `limits` object. `[gateway] on_store_error` defaults to `"closed"`, so a counter-store failure refuses admission rather than letting unlimited traffic through; self-service `GET /v1/usage` has its own `[gateway] usage_rpm` budget. See [shaping](shaping.md) and [configuration](configuration.md).

The management API cannot set `allowed_upstreams` or `allowed_models`: those glob allowlists belong to statically configured `[streams.NAME]` entries. Keep credential-bearing routes on static streams when a tenant must be confined to particular upstreams or models. Per-stream `weight` is also a static-stream setting and controls fair sharing of contested keys.

## Verify the audit log

Every write produces a JSONL record with a sequence number, UTC time, actor hash, source, action, stream, outcome, reason, field-level changes, a link to the previous hash and the record hash. Reasons are sanitized before they are written; raw tokens, raw actor ids and provider key values never enter the log. There is no HTTP audit route: verification is offline, on a copy of the files.

```sh
reiki streams audit verify /etc/reiki/stream-admin-audit.jsonl
reiki streams audit head /etc/reiki/stream-admin-audit.jsonl --n 5
```

`verify` prints `ok: N audit record(s), chain verified` on success and exits `0`. A changed record is reported as `hash mismatch`, `sequence gap`, `invalid record` or `audit line N: broken chain`; a missing sidecar for a non-empty log is `audit head missing`, and tail deletion or truncation is `audit head mismatch: records were removed or truncated`. Verification errors exit `2`. The sibling `audit.head` sidecar holds the last sequence number and hash; keep it beside the JSONL file or tail deletion will not be detected, and an empty log without a sidecar verifies as zero records. Both commands need the JSONL file, but no running instance or token.

## Backup and restore

The dynamic state file is written atomically with mode `0600`. It lives in the state directory beside `<config>.state`; with the default name and `/etc/reiki/reiki.toml`, that is `/etc/reiki/stream-admin.json`. The audit log is `/etc/reiki/stream-admin-audit.jsonl` in the same directory, with its sidecar `/etc/reiki/stream-admin-audit.head`. Both names are relative in the config and may be changed with `state_file` and `audit_log`. See [backup and restore](deploy.md#backup-and-restore) for the full file table.

To back up, stop the instance (or copy immediately after a successful write) and copy these three files together, keeping owner and mode:

```sh
systemctl stop reiki
install -d -m 0700 /var/backups/reiki/stream-admin
cp -p /etc/reiki/stream-admin.json /var/backups/reiki/stream-admin/
cp -p /etc/reiki/stream-admin-audit.jsonl /var/backups/reiki/stream-admin/
cp -p /etc/reiki/stream-admin-audit.head /var/backups/reiki/stream-admin/
systemctl start reiki
```

Never edit or truncate a live audit log or sidecar; management fails closed when a record cannot be appended. Verify an archived copy offline with `reiki streams audit verify` before treating it as recoverable.

To restore, stop the instance, put the three files back with mode `0600`, then start it and check `GET /v1/status` for `stream_admin.state_error` and `reiki streams list` for the expected streams. A fresh audit file starts a new chain at record 1, so keep old JSONL and sidecar pairs for earlier periods. An invalid state file drops dynamic streams while static streams keep working; restore the state file from backup rather than editing it.
