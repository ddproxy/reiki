# Documentation

Reiki's documentation is one page per subsystem. Commands and flags are checked against the CLI definitions, config keys against the config code, and behaviour against the source and tests.

## Read in this order

1. [Overview](overview.md) — what Reiki is, the listeners and the request path. Use the [glossary](glossary.md) for CLI and status vocabulary.
2. [Configuration](configuration.md) and [command line](cli.md) — every table and flag. Then [keys and streams](keys.md) — key files, token hashes and chains.
3. [Providers](providers.md) — upstream mapping and the provider status table. Then [failover](failover.md) (chains and key states) and [classification](classification.md) (response rules and cooldowns).
4. [Security](security.md) — assets and trust boundaries. Then the [redaction reference](redaction.md) for exact forms, and [plugins](plugins.md), [placeholders](placeholders.md), [PHI](phi.md), [finance](finance.md) and [controlled information](controlled.md) for egress controls.
5. [Request log](requestlog.md), [monitoring](monitoring.md), [troubleshooting](troubleshooting.md) and the [FAQ](faq.md) — observe a running instance and diagnose a symptom.
6. [Tunnel](tunnel.md), [shaping](shaping.md), [peers](peers.md), [reiki top](top.md) and [hub](hub.md) — optional subsystems.
7. [Deploy](deploy.md) and [upgrades](upgrades.md) — run and change a service. [Examples](examples.md) shows verified command output formats for common tasks.

## All pages

| Group | Pages |
| --- | --- |
| Start | [Overview](overview.md), [architecture](architecture.md), [glossary](glossary.md), [command line](cli.md), [configuration](configuration.md), [keys and streams](keys.md) |
| Reliability | [Failover](failover.md), [classification](classification.md), [shaping](shaping.md), [providers](providers.md) |
| Security and data | [Security](security.md), [redaction reference](redaction.md), [plugins](plugins.md), [plugin authoring](plugin-authoring.md), [placeholders](placeholders.md), [PHI and NPPI](phi.md), [finance](finance.md), [controlled information](controlled.md), [tunnel](tunnel.md), [request log](requestlog.md) |
| Operate | [Deploy](deploy.md), [service redirect](service.md), [upgrades](upgrades.md), [monitoring](monitoring.md), [peers](peers.md), [peers publishing](peers-publishing.md), [reiki top](top.md), [troubleshooting](troubleshooting.md), [hub](hub.md) |
| Help | [Examples](examples.md), [FAQ](faq.md) |

The [changelog](../CHANGELOG.md) announces behaviour changes by version heading and subsection. There is no separate schema version for the request log; see its [stability note](requestlog.md#stability-of-records).
