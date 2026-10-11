# Reiki

Reiki is a reverse proxy that holds several upstream API keys and switches to the next one when a key hits a rate limit, runs out of quota or is disabled. Clients authenticate with their own token and never see an upstream key.

## Install

```bash
cargo build --release
install -m 0755 target/release/reiki /usr/local/bin/
```

## Configure

Reiki reads a TOML file, `/etc/reiki/reiki.toml` by default (`--config` to change it).

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

Setup for OpenAI, OpenCode Zen, Anthropic, Gemini and other providers is in [docs/providers.md](docs/providers.md).

`GET /__reiki/healthz` returns 200 when Reiki is up.

## Failover

| Response | Action |
| --- | --- |
| 429 | Cool the key for `Retry-After`, or 60 s |
| Quota exhausted, 402, or 400 with an empty-balance body | Cool the key for 5 h |
| Access disabled | Try the next key for that request only; the key stays in rotation |
| 401, or 400 with an invalid-key body | Take the key out of rotation until Reiki restarts |
| 5xx, timeout | Retry once, then cool the key for 30 s |
| Other 4xx | Pass through unchanged |

A failed request replays on the next key as long as no response bytes have reached the client. If every key is out, Reiki returns 503, with `Retry-After` when a key will recover.

## Develop

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
