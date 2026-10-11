<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.png">
    <img src="docs/assets/logo.png" alt="Reiki logo" width="200">
  </picture>
</p>

# Reiki

Reiki is a reverse proxy that holds several upstream API keys and switches to the next one when a key hits a rate limit, runs out of quota or is disabled. Clients authenticate with their own token and never see an upstream key.

## Install

Download the archive for your platform from the [releases page](https://github.com/ddproxy/reiki/releases). Builds cover Linux (static musl) and macOS, on x86_64 and aarch64.

```bash
VERSION=v0.1.1
TARGET=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl, aarch64-apple-darwin, x86_64-apple-darwin
curl -fsSLO https://github.com/ddproxy/reiki/releases/download/$VERSION/reiki-$VERSION-$TARGET.tar.gz
curl -fsSL https://github.com/ddproxy/reiki/releases/download/$VERSION/reiki-$VERSION-$TARGET.tar.gz.sha256 | shasum -a 256 -c
tar xzf reiki-$VERSION-$TARGET.tar.gz
install -m 0755 reiki-$VERSION-$TARGET/reiki /usr/local/bin/
```

To build from source instead, install Rust 1.85 or newer and run `cargo install --git https://github.com/ddproxy/reiki --locked`.

## Configure

Reiki reads a TOML file, `/etc/reiki/reiki.toml` by default (`--config` to change it). Override the `listen` address with `--listen <addr>` or `REIKI_LISTEN`; the flag wins over the variable, and both win over the file. `reiki check` prints the address Reiki will use.

```toml
listen = "127.0.0.1:4097"

[upstream.zen]
base_url = "https://opencode.ai/zen/v1"   # must be https
# auth_header = "authorization"           # defaults
# auth_format = "Bearer {key}"

[keys.zen-a]
upstream = "zen"
file = "/etc/reiki/keys/zen-a"            # file holding the key

[keys.zen-b]
upstream = "zen"
file = "/etc/reiki/keys/zen-b"

[streams.laptop]
token_sha256 = "<sha256 of the client token>"
chain = ["zen-a", "zen-b"]                # tried in order
strategy = "sticky"                       # or "round_robin"
```

- A **stream** is one client with its own token and key chain. Streams never share cooldown state unless they list the same key.
- Keys live in files, never in the config. Keep them mode 0600.
- Generate the token hash with `printf '%s' "$TOKEN" | reiki hash-token`.
- Validate the setup with `reiki check`, then start Reiki with `reiki`.

## Use

Point the client's base URL at `http://127.0.0.1:4097/<upstream>` and send the stream token where the client would normally put its API key. The path prefix picks the upstream, so `/zen/chat/completions` goes to `https://opencode.ai/zen/v1/chat/completions`. Only the credential header is replaced; every other header passes through.

Clients send the stream token as `Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or a `?key=` query parameter, in that order of preference. Setup for OpenAI, OpenCode Zen, Anthropic, Gemini and other providers is in [docs/providers.md](docs/providers.md).

`GET /__reiki/healthz` returns 200 and `ok` when Reiki is up. It needs no token.

## Failover

| Upstream response | Action |
| --- | --- |
| 429 | Cool the key for `Retry-After`, or 60 s |
| 402, or 429 with a quota phrase in the body (`quota`, `usage exceeded`, `insufficient`) | Cool the key for 5 h |
| 400 with an empty-balance body | Cool the key for 5 h |
| 403 or 5xx saying access is disabled | Try the next key for that request only; the key stays in rotation |
| 5xx saying rate limit | Cool the key for `Retry-After`, or 60 s |
| 401, other 403, or 400 with an invalid-key body | Take the key out of rotation until Reiki restarts or you rotate the key file and reload |
| Other 5xx, connection error, timeout | Retry once on the same key, then cool it for 30 s after the second failure in a row |
| 403 that is a CDN block page | Pass through unchanged |
| Other 4xx | Pass through unchanged |

A failed request replays on the next key as long as no response bytes have reached the client. If every key is out, Reiki returns 503 `reiki_exhausted`, with `Retry-After` when a key will recover.

`Retry-After` is read as delta-seconds or as an HTTP-date (IMF-fixdate, such as `Sun, 06 Nov 1994 08:49:37 GMT`); a cooldown taken from it is capped at 24 h.

Limits: request bodies up to 8 MB, 120 s to receive upstream response headers. Reiki reads the config and key files at startup and again on SIGHUP, so send `kill -HUP <pid>` after changing a key file or the config. In-flight requests keep running on the version they started with. A changed `listen` is ignored until you restart Reiki.

See [docs/troubleshooting.md](docs/troubleshooting.md) for status codes and logs, and [docs/security.md](docs/security.md) for the security model.

## Develop

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
