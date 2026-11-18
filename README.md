<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.png">
    <img src="docs/assets/logo.png" alt="Reiki logo" width="200">
  </picture>
</p>

# Reiki

Reiki is a security proxy for coding-agent and LLM traffic. It sits between a client and the providers it calls, keeps upstream API keys in files only Reiki reads, and gives the client a revocable stream token instead. Requests pass through one place where Reiki can rotate and fail over keys, filter outbound content and expose health and logs. Reiki runs as one binary, reads one TOML file and compiles plugins in at build time.

## Is it for you

| Situation | What Reiki does |
| --- | --- |
| An agent or application would otherwise hold provider API keys | The client sends a Reiki stream token; Reiki reads the upstream key from a file and sets the credential per request. |
| You need to keep one key working across rate limits, quota failures or a bad key | A stream chain can try other keys and, when configured, a fallback upstream. |
| You need to limit or inspect what leaves the machine | Upstreams come from the config, plugins can mask or block content, and the request log and metrics show what happened. |
| You run several instances and need a shared view of key health | Peers publish a read-only health summary that other instances and `reiki top` can read. |

Start with the [overview](docs/overview.md) for how the parts fit together, or the [architecture](docs/architecture.md) for the request path.

## Install

From a checkout (Rust 1.99 or newer), `make install` builds with `reiki top` and puts `reiki` in `~/.local/bin`, or in `/usr/local/bin` as root. `PREFIX=/opt/reiki make install` picks another prefix.

Or download a prebuilt archive from the [releases page](https://github.com/ddproxy/reiki/releases) (Linux static musl and macOS, x86_64 and aarch64; `reiki top` included), check it against its `.sha256` file and copy `reiki` onto your `PATH`.

Plugin selection and order are build arguments; see [plugins](docs/plugins.md#choose-and-order-plugins-at-build-time). To write a plugin, see [plugin authoring](docs/plugin-authoring.md). For service files and first-start checks, see [deploy](docs/deploy.md).

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

- [Overview](docs/overview.md): how the parts fit together.
- [Architecture](docs/architecture.md): request path, key states and source map.
- [Glossary](docs/glossary.md): terms and CLI/status vocabulary.
- [Configuration](docs/configuration.md): every table and field.
- [CLI](docs/cli.md): every subcommand.
- [Keys and streams](docs/keys.md): key files, token hashes, chains and rotation.

**Keys and providers**

- [Providers](docs/providers.md): upstreams, transport, forwarding, fallback and passthrough.
- [Failover](docs/failover.md): chains and key states.
- [Classification](docs/classification.md): response rules, one cooldown table and client errors.
- [Shaping](docs/shaping.md): outbound budgets and limits.

**Plugins and egress**

- [Plugins](docs/plugins.md): the plugin model and the bundled set.
- [Plugin authoring](docs/plugin-authoring.md): hooks, tests and build-time registration.
- [Placeholders](docs/placeholders.md), [PHI and NPPI](docs/phi.md), [Finance](docs/finance.md), [Controlled information](docs/controlled.md).
- [Request log](docs/requestlog.md).
- [Tunnel](docs/tunnel.md): CONNECT proxy, destination allow-list and interception.

**Operations and security**

- [Deploy](docs/deploy.md): install paths, directory layout, listeners and a service unit.
- [Upgrades](docs/upgrades.md): reload and restart rules, state compatibility and rollback.
- [Monitoring](docs/monitoring.md): the admin listener, `/status`, `/metrics` and key checks.
- [Peers](docs/peers.md): read-only key health shared between instances; health document schema.
- [Peers publishing](docs/peers-publishing.md): listener, tokens and stream names.
- [Top](docs/top.md): the live fleet view.
- [Hub](docs/hub.md): hub configuration, parsed and validated in this release.
- [Troubleshooting](docs/troubleshooting.md): status codes, logs, reload and common problems.
- [Security](docs/security.md): protections, limits and trust boundaries.

## Develop

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
