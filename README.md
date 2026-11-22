<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.png">
    <img src="docs/assets/logo.png" alt="Reiki logo" width="200">
  </picture>
</p>

# Reiki

[![ci](https://github.com/ddproxy/reiki/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/ddproxy/reiki/actions/workflows/ci.yml)
[![CodeQL](https://github.com/ddproxy/reiki/actions/workflows/codeql.yml/badge.svg)](https://github.com/ddproxy/reiki/actions/workflows/codeql.yml)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/ddproxy/reiki/badge)](https://scorecard.dev/viewer/?uri=github.com/ddproxy/reiki)
[![Release](https://img.shields.io/github/v/release/ddproxy/reiki)](https://github.com/ddproxy/reiki/releases/latest)
[![License](https://img.shields.io/github/license/ddproxy/reiki)](LICENSE)

Reiki is a security proxy for coding-agent and LLM traffic. It sits between a client and the providers it calls, keeps upstream API keys in files on the Reiki host, and gives the client a revocable stream token instead. Requests pass through one place where Reiki can rotate and fail over keys, filter outbound content and expose health and logs. Reiki runs as one binary, reads one TOML file and compiles plugins in at build time.

## Is it for you

| Situation | What Reiki does |
| --- | --- |
| An agent or application would otherwise hold provider API keys | The client sends a Reiki stream token; Reiki reads the upstream key from a file and sets the credential per request. |
| You need to keep one key working across rate limits, quota failures or a bad key | A stream chain can try other keys and, when configured, a fallback upstream. |
| You need to limit or inspect what leaves the machine | Upstreams come from the config, plugins can mask or block content, and the request log and metrics show what happened. |
| You run several instances and need a shared view of key health | Peers publish a read-only health summary that other instances and `reiki top` can read. |

## Read in this order

1. [Overview](docs/overview.md) — the model and the listeners. Use the [glossary](docs/glossary.md) for CLI and status vocabulary.
2. [Configuration](docs/configuration.md) and [command line](docs/cli.md) — every table and flag; then [keys and streams](docs/keys.md).
3. [Providers](docs/providers.md) — mapping and test coverage; then [failover](docs/failover.md) and [classification](docs/classification.md).
4. [Security](docs/security.md) — assets and trust boundaries; then the [redaction reference](docs/redaction.md) and the egress plugins: [placeholders](docs/placeholders.md), [PHI](docs/phi.md), [finance](docs/finance.md), [controlled information](docs/controlled.md), [tool policy](docs/toolpolicy.md).
5. [Request log](docs/requestlog.md), [monitoring](docs/monitoring.md), [troubleshooting](docs/troubleshooting.md) and the [FAQ](docs/faq.md) — observe and diagnose.
6. [Tunnel](docs/tunnel.md), [shaping](docs/shaping.md), [peers](docs/peers.md), [reiki top](docs/top.md) and [hub](docs/hub.md) — optional subsystems.
7. [Deploy](docs/deploy.md), [upgrades](docs/upgrades.md) and [worked examples](docs/examples.md) — run, change and see the output.

The full index is [docs/README.md](docs/README.md).

## Install

From a checkout (Rust 1.99 or newer), `make install` builds with `reiki top` and puts `reiki` in `~/.local/bin`, or in `/usr/local/bin` as root. `PREFIX=/opt/reiki make install` picks another prefix.

Or download a prebuilt archive from the [releases page](https://github.com/ddproxy/reiki/releases) (Linux static musl and macOS, x86_64 and aarch64; `reiki top` included), check it against its `.sha256` file and copy `reiki` onto your `PATH`.

Plugin selection and order are build arguments; see [plugins](docs/plugins.md#choose-and-order-plugins-at-build-time). To write a plugin, see [plugin authoring](docs/plugin-authoring.md). For service files and first-start checks, see [deploy](docs/deploy.md).

## Security

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md). [Security scanning](docs/security-scanning.md) lists the repository checks, where their findings appear and the settings the owner must enable.

## Verify a release

Archives ship with `.sha256` files, a CycloneDX SBOM and build provenance attestations. Check the one you use:

```bash
tag=v0.14.0
gh release download "$tag" --repo ddproxy/reiki \
  --pattern '*.tar.gz' --pattern '*.sha256' --pattern '*-sbom.json' --dir dist
gh attestation verify "dist/reiki-$tag-x86_64-unknown-linux-musl.tar.gz" --repo ddproxy/reiki
(cd dist && shasum -a 256 -c *.sha256)
```

The `.sha256` files cover the archives and `reiki-$tag-sbom.json`; the SBOM lists the components in the release build.

## Quick start

This sets up one upstream, one key, one stream and an admin listener so you can see the result.

1. Write the provider key to a file that only Reiki's user can read.

   ```bash
   umask 077
   printf '%s' "$PROVIDER_KEY" > /path/to/keys/example-a
   ```

2. Choose a client token and an admin token (each at least 16 characters) and hash them.

   ```bash
   printf '%s' "$CLIENT_TOKEN" | reiki hash-token
   printf '%s' "$ADMIN_TOKEN"  | reiki hash-token
   ```

3. Save `reiki.toml` with the hashes from step 2.

   ```toml
   listen = "127.0.0.1:4097"

   [upstream.example]
   base_url = "https://api.example.com/v1"

   [keys.example-a]
   upstream = "example"
   file = "/path/to/keys/example-a"

   [streams.client-a]
   token_sha256 = "<hash of $CLIENT_TOKEN>"
   chain = ["example-a"]

   [admin]
   listen = "127.0.0.1:4098"
   token_sha256 = "<hash of $ADMIN_TOKEN>"
   ```

4. Check the setup, then run Reiki.

   ```bash
   reiki --config reiki.toml check
   reiki --config reiki.toml
   ```

5. Point one agent at Reiki and send a request. For OpenCode, set `provider.opencode.options.baseURL` to `http://127.0.0.1:4097/example` and `apiKey` to `$CLIENT_TOKEN` (see [Providers](docs/providers.md#opencode-zen)). You can also call Reiki directly:

   ```bash
   curl -s http://127.0.0.1:4097/example/chat/completions \
     -H "Authorization: Bearer $CLIENT_TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"model":"example-model","messages":[{"role":"user","content":"ping"}]}'
   ```

6. See the result from the running instance.

   ```bash
   REIKI_ADMIN_TOKEN=$ADMIN_TOKEN reiki keys
   REIKI_ADMIN_TOKEN=$ADMIN_TOKEN reiki failures --limit 5
   ```

`reiki keys` shows each key's state and last status. `reiki failures` shows recent failures from the in-memory ring. For a real provider, replace `example`, `base_url` and `model` with values from [Providers](docs/providers.md).

## Documentation

**Start**

- [Quickstart](docs/quickstart.md): ten-minute proof-first setup and negative test.
- [Documentation index](docs/README.md): the numbered reading path and every page.
- [Overview](docs/overview.md): how the parts fit together.
- [Architecture](docs/architecture.md): request path, key states and source map.
- [Glossary](docs/glossary.md): terms and CLI/v1/status vocabulary.
- [Configuration](docs/configuration.md): every table and field.
- [CLI](docs/cli.md): every subcommand.
- [Keys and streams](docs/keys.md): key files, token hashes, chains and rotation.

**Keys and providers**

- [Providers](docs/providers.md): upstreams, transport, forwarding, fallback and passthrough.
- [OpenCode](docs/opencode.md): base URL, client token, proof check and bypasses.
- [Claude Code](docs/claude-code.md): base URL, client token, proof check and bypasses.
- [Failover](docs/failover.md): chains and key states.
- [Classification](docs/classification.md): response rules, one cooldown table and client errors.
- [Shaping](docs/shaping.md): outbound budgets and limits.

**Plugins and egress**

- [Plugins](docs/plugins.md): the plugin model and the bundled set.
- [Plugin authoring](docs/plugin-authoring.md): hooks, tests and build-time registration.
- [Placeholders](docs/placeholders.md), [PHI and NPPI](docs/phi.md), [Finance](docs/finance.md), [Controlled information](docs/controlled.md), [Tool policy](docs/toolpolicy.md).
- [Request log](docs/requestlog.md): records and their stability note.
- [Redaction reference](docs/redaction.md): the exact forms scrubbed from relayed bodies and headers.
- [Tunnel](docs/tunnel.md): CONNECT proxy, destination allow-list and interception.
- [Worked examples](docs/examples.md): verified command output formats for PHI, placeholders, quotas, tunnel denials, reload and rotation.

**Operations and security**

- [Deploy](docs/deploy.md): install paths, directory layout, listeners and a service unit.
- [Upgrades](docs/upgrades.md): reload and restart rules and rollback.
- [Monitoring](docs/monitoring.md): the admin listener, `/v1/status`, `/v1/metrics` and key checks.
- [Peers](docs/peers.md): read-only key health shared between instances; health document schema.
- [Peers publishing](docs/peers-publishing.md): listener, tokens and stream names.
- [Top](docs/top.md): the live fleet view.
- [Hub](docs/hub.md): hub configuration, parsed and validated in this release.
- [Troubleshooting](docs/troubleshooting.md): status codes, logs, reload and common problems.
- [FAQ](docs/faq.md): short answers to the questions asked most often.
- [Security](docs/security.md): protections, limits and trust boundaries.
- [Security scanning](docs/security-scanning.md): repository checks, findings and release verification.
- [Observe, then enforce](docs/rollout.md): placeholders, report mode, `reiki scan` and the reload gate.
- [What Reiki does not protect against](docs/limits.md): key-file modes, plugins, tunnels and encodings.

## Develop

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
