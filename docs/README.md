# Documentation

Reiki's documentation is one page per subsystem. Commands and flags are checked against the CLI definitions, config keys against the config code, and behaviour against the source and tests.

## Read in this order

1. [Install](install.md) — the tuishou one-liner status, the manual path that works today, and release verification. Then [Overview](overview.md) — what Reiki is, the listeners and the request path. Use the [glossary](glossary.md) for CLI and status vocabulary.
2. [Configuration](configuration.md) and [command line](cli.md) — every table and flag, including the opt-in `[stream_admin]` table. Then [keys and streams](keys.md) — key files, token hashes, chains and dynamic stream credentials.
3. [Providers](providers.md) — upstream mapping and the provider status table. Then [failover](failover.md) (chains and key states) and [classification](classification.md) (response rules and cooldowns).
4. [Security](security.md) — assets, trust boundaries and dynamic stream administration, and [security scanning](security-scanning.md) — repository checks and release verification. Then the [redaction reference](redaction.md) for exact forms, and [plugins](plugins.md), [placeholders](placeholders.md), [PHI](phi.md), [finance](finance.md), [controlled information](controlled.md), [tool policy](toolpolicy.md) and [cost](cost.md) for egress controls and opt-in accounting.
5. [Request log](requestlog.md), [monitoring](monitoring.md), [troubleshooting](troubleshooting.md) and the [FAQ](faq.md) — observe a running instance and diagnose a symptom.
6. [Tunnel](tunnel.md), [shaping](shaping.md), [peers](peers.md), [reiki top](top.md) and [hub](hub.md) — optional subsystems.
7. [Deploy](deploy.md), [operations](operations.md) and [upgrades](upgrades.md) — run, back up and change a service. [Dynamic streams](dynamic-streams.md) is the operator runbook for create, rotate, revoke, expiry, limits and audit. [Examples](examples.md) shows verified command output formats for common tasks.

## Stream admin API

`[stream_admin]` is off unless configured. With it, the admin listener serves these routes with a dedicated stream-management token, not the ordinary admin token. For create, rotate, revoke, expiry, limits, audit and backup procedures, see the [dynamic streams runbook](dynamic-streams.md).

| Method | Route | Purpose |
| --- | --- | --- |
| `GET` | `/v1/streams` | List dynamic streams. |
| `GET` | `/v1/streams/{name}` | Show one dynamic stream. |
| `PUT` | `/v1/streams/{name}` | Update its assignable key or limits. |
| `DELETE` | `/v1/streams/{name}` | Delete a dynamic stream. |
| `POST` | `/v1/streams/create` | Create a stream and return its first token once. |
| `POST` | `/v1/streams/{name}/tokens` | Add a token and return it once. |
| `POST` | `/v1/streams/{name}/tokens/{token_id}/revoke` | Revoke one token. |
| `POST` | `/v1/streams/{name}/revoke` | Revoke every token on a stream. |

Writes need an `Idempotency-Key` header and a `reason` in strict JSON; unknown fields are refused, and `X-Confirm-Token` is required when two-party confirmation is configured. Audit records are read offline with the CLI, never over HTTP. See [monitoring](monitoring.md#stream-administration-routes) for the route reference and [configuration](configuration.md#stream_admin) for the settings.

The CLI reads the raw token from the configured environment variable or `--token-file`; it is never an argument. `create` and `token add` print a new token once.

```bash
export REIKI_STREAM_ADMIN_TOKEN="$(cat /etc/reiki/stream-admin.token)"

reiki --config /etc/reiki/reiki.toml streams list
reiki --config /etc/reiki/reiki.toml streams show tenant-a
reiki --config /etc/reiki/reiki.toml streams --reason "ticket-4711" create tenant-a zen-a --ttl 3600
reiki --config /etc/reiki/reiki.toml streams --reason "ticket-4712" update tenant-a --key zen-b --rpm 90
reiki --config /etc/reiki/reiki.toml streams --reason "ticket-4713" token add tenant-a
reiki --config /etc/reiki/reiki.toml streams --reason "ticket-4714" token revoke tenant-a TOKEN_ID_FROM_SHOW
reiki --config /etc/reiki/reiki.toml streams --reason "ticket-4715" revoke tenant-a

reiki streams audit verify /etc/reiki/stream-admin-audit.jsonl
reiki streams audit head /etc/reiki/stream-admin-audit.jsonl --n 5
```

Keep `stream-admin-audit.head` beside the JSONL file: when it is present, `audit verify` uses it to detect tail deletion or truncation. Without it, verification refuses a non-empty log.

## All pages

| Group | Pages |
| --- | --- |
| Start | [Quickstart](quickstart.md), [install](install.md), [overview](overview.md), [architecture](architecture.md), [glossary](glossary.md), [command line](cli.md), [configuration](configuration.md), [keys and streams](keys.md) |
| Reliability | [Failover](failover.md), [classification](classification.md), [shaping](shaping.md), [providers](providers.md), [OpenCode](opencode.md), [Claude Code](claude-code.md) |
| Security and data | [Security](security.md), [security scanning](security-scanning.md), [redaction reference](redaction.md), [plugins](plugins.md), [plugin authoring](plugin-authoring.md), [placeholders](placeholders.md), [PHI and NPPI](phi.md), [finance](finance.md), [controlled information](controlled.md), [tool policy](toolpolicy.md), [cost](cost.md), [tunnel](tunnel.md), [request log](requestlog.md) |
| Operate | [Operations](operations.md), [dynamic streams](dynamic-streams.md), [deploy](deploy.md), [upgrades](upgrades.md), [monitoring](monitoring.md), [peers](peers.md), [peers publishing](peers-publishing.md), [reiki top](top.md), [troubleshooting](troubleshooting.md), [hub](hub.md) |
| Help | [Examples](examples.md), [FAQ](faq.md), [limits](limits.md), [observe, then enforce](rollout.md), [AGENTS](../AGENTS.md), [changelog](../CHANGELOG.md) |

The [changelog](../CHANGELOG.md) announces behaviour changes by version heading and subsection. There is no separate schema version for the request log; see its [stability note](requestlog.md#stability-of-records).
