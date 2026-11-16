<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.png">
    <img src="docs/assets/logo.png" alt="Reiki logo" width="200">
  </picture>
</p>

# Reiki

Security proxy for agent and LLM traffic: isolates keys, filters what leaves, keeps access reliable.

Reiki sits between a client and the LLM providers it calls. The client can be an agent such as opencode or Claude Code, or an application that would otherwise hold provider API keys. The client gets a stream token. Reiki keeps the keys.

Start with the [overview](docs/overview.md) for how the parts fit together.

## Isolation

Upstream keys and secrets stay out of the client. Keys live in files that only Reiki reads. The client authenticates with a revocable stream token. See [security](docs/security.md#clients-and-keys) for what this does and does not cover, and [configuration](docs/configuration.md) for the key and stream tables.

## Control

Reiki filters what leaves. The `placeholders` plugin masks credentials and listed terms. The `phi`, `finance` and `controlled` plugins block or report identifiers. Upstreams come only from the config, and the optional tunnel allows only listed destinations. Read [placeholders](docs/placeholders.md), [phi](docs/phi.md), [finance](docs/finance.md), [controlled](docs/controlled.md) and [tunnel](docs/tunnel.md).

## Reliability and visibility

Access stays up and you can see it. A stream's chain of keys, and an optional fallback upstream, takes over on a rate limit, an exhausted quota or a disabled key ([failover](docs/failover.md)). Traffic shaping ([shaping](docs/shaping.md)) keeps each key inside its budget. The request log ([requestlog](docs/requestlog.md)), `reiki top` ([top](docs/top.md)), peers ([peers](docs/peers.md)) and `/metrics` ([monitoring](docs/monitoring.md)) show what happened.

## Install

From a checkout (Rust 1.99 or newer), `make install` builds with `reiki top` and puts `reiki` in `~/.local/bin`, or in `/usr/local/bin` as root. `PREFIX=/opt/reiki make install` picks another prefix.

Or download a prebuilt archive from the [releases page](https://github.com/ddproxy/reiki/releases) (Linux static musl and macOS, x86_64 and aarch64; `reiki top` included), check it against its `.sha256` file and copy `reiki` onto your `PATH`.

Plugin selection and order are build arguments; see [docs/plugins.md](docs/plugins.md#choose-and-order-plugins-at-build-time).

## Quick start

This takes one upstream, one key and one stream. Replace each placeholder.

1. Write the provider key to a file that only Reiki's user can read.

   ```bash
   umask 077
   printf '%s' "$PROVIDER_KEY" > /path/to/keys/example-a
   ```

2. Choose a client token of at least 16 characters and hash it.

   ```bash
   printf '%s' "$CLIENT_TOKEN" | reiki hash-token
   ```

3. Save `reiki.toml`, with the hash from step 2 in `token_sha256`.

   ```toml
   [upstream.example]
   base_url = "https://api.example.com/v1"

   [keys.example-a]
   upstream = "example"
   file = "/path/to/keys/example-a"

   [streams.client-a]
   token_sha256 = "<64 hex characters from hash-token>"
   chain = ["example-a"]
   ```

4. Check the setup, then run Reiki. It listens on `127.0.0.1:4097` unless `listen` says otherwise.

   ```bash
   reiki --config reiki.toml check
   reiki --config reiki.toml
   ```

5. From another terminal, send a request through Reiki. Only the stream token goes to Reiki.

   ```bash
   curl -s http://127.0.0.1:4097/example/chat/completions \
     -H "Authorization: Bearer $CLIENT_TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"model": "example-model", "messages": [{"role": "user", "content": "ping"}]}'
   ```

To point a client at Reiki, set its base URL to `http://127.0.0.1:4097/<upstream>` and send the stream token where it would send its API key. Clients may send the token as `Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or a `?key=` query parameter, in that order of preference. Provider setup is in [providers](docs/providers.md).

## Documentation

**Start**

- [Overview](docs/overview.md): the model, the three jobs and a glossary.
- [Configuration](docs/configuration.md): every table and field.
- [CLI](docs/cli.md): every subcommand.

**Keys and providers**

- [Providers](docs/providers.md): upstreams, transport, forwarding, fallback and passthrough.
- [Failover](docs/failover.md): chains, cooldown classes, `Retry-After` and `quota_resets`.
- [Shaping](docs/shaping.md): outbound budgets and limits.

**Plugins**

- [Plugins](docs/plugins.md): the plugin model and the bundled set.
- [Placeholders](docs/placeholders.md), [PHI and NPPI](docs/phi.md), [Finance](docs/finance.md), [Controlled information](docs/controlled.md).
- [Request log](docs/requestlog.md).

**Egress**

- [Tunnel](docs/tunnel.md): CONNECT proxy, destination allow-list and interception.

**Operations**

- [Monitoring](docs/monitoring.md): the admin listener, `/status`, `/metrics` and key checks.
- [Peers](docs/peers.md): read-only key health shared between instances.
- [Top](docs/top.md): the live fleet view.
- [Hub](docs/hub.md): hub configuration, parsed only in this release.
- [Service](docs/service.md): signals, systemd and an example unit.
- [Troubleshooting](docs/troubleshooting.md): status codes, logs, reload and common problems.

**Security**

- [Security](docs/security.md): what Reiki protects and what it does not.

## Develop

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
