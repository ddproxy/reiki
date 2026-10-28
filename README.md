<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.png">
    <img src="docs/assets/logo.png" alt="Reiki logo" width="200">
  </picture>
</p>

# Reiki

Reiki is a security proxy between a client and the LLM providers it calls. The client can be an agent such as opencode or Claude Code, or an application that would otherwise hold provider API keys. Reiki keeps upstream keys away from the client, and keeps credentials and sensitive terms in the client's prompts away from the provider.

- **Key isolation.** Upstream keys live in files only Reiki reads. The client authenticates with a revocable stream token and never sees a key. Reiki redacts a key that an upstream echoes back in an error; [security](docs/security.md#clients-and-keys) lists the exception.
- **Secret and term masking.** The `placeholders` plugin swaps credentials and a list of sensitive terms in a request for stable placeholders before it reaches the provider, and restores them in the response. Private keys are removed outright, and credentials stay masked inside tool-call arguments by default.
- **Controlled egress.** Upstreams come only from the config, and the optional CONNECT tunnel allows only listed destinations. Interception can bring chosen hosts under the same rules, with a name-constrained CA.
- **Auditable traffic.** The `requestlog` plugin records what the client sent and received, with credentials masked, to a file and a live socket. Failed logins are logged and throttled.
- **Key failover.** A stream's chain of keys, and optionally a fallback upstream, takes over on a rate limit, exhausted quota or disabled key, so the client sees one stable endpoint.

See [docs/security.md](docs/security.md) for what Reiki does and does not protect against.

## Install

Download the archive for your platform from the [releases page](https://github.com/ddproxy/reiki/releases). Builds cover Linux (static musl) and macOS, on x86_64 and aarch64.

```bash
VERSION=v0.7.1
TARGET=x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl, aarch64-apple-darwin, x86_64-apple-darwin
curl -fsSLO https://github.com/ddproxy/reiki/releases/download/$VERSION/reiki-$VERSION-$TARGET.tar.gz
curl -fsSL https://github.com/ddproxy/reiki/releases/download/$VERSION/reiki-$VERSION-$TARGET.tar.gz.sha256 | shasum -a 256 -c
tar xzf reiki-$VERSION-$TARGET.tar.gz
install -m 0755 reiki-$VERSION-$TARGET/reiki /usr/local/bin/
```

To build from source instead, install Rust 1.99 or newer and run `cargo install --git https://github.com/ddproxy/reiki --locked`. Plugin selection and order are build arguments; see [docs/plugins.md](docs/plugins.md#choose-and-order-plugins-at-build-time).

## Configure

Reiki reads a TOML file, `/etc/reiki/reiki.toml` by default (`--config` to change it). Override the `listen` address with `--listen <addr>` or `REIKI_LISTEN`; the flag wins over the variable, and both win over the file. `reiki check` prints the address Reiki will use.

```toml
listen = "127.0.0.1:4097"

[upstream.example]
base_url = "https://api.example.com/v1"   # must be https
# auth_header = "authorization"           # defaults
# auth_format = "Bearer {key}"
# model_aliases = { "small" = "vendor/small-v2" }  # rewrite the top-level `model`
# drop_params = ["stream_options"]        # remove these top-level keys
# http2 = true                            # negotiate HTTP/2 (default: HTTP/1.1)
# forward_accept_encoding = true          # send the client's Accept-Encoding (default: identity)
# title_case_headers = true               # send Content-Type rather than content-type (HTTP/1.1)
# send_keep_alive = true                  # add Connection: keep-alive
# [upstream.example.forward]              # optional: hand requests to a listener or command
# socket = "/run/forwarder.sock"          # or `command` with optional `args` and `pass_env`
# # expect_uid = 1000                     # socket only

[keys.example-a]
upstream = "example"
file = "/etc/reiki/keys/example-a"            # file holding the key
# quota_resets = "daily 00:00 UTC"        # end a quota cooldown at the next reset

[keys.example-b]
upstream = "example"
file = "/etc/reiki/keys/example-b"

[streams.laptop]
token_sha256 = "<sha256 of the client token>"
chain = ["example-a", "example-b"]                # tried in order
strategy = "sticky"                       # or "round_robin"
# rate_limit_per_minute = 60              # optional; off unless set

[admin]                                    # optional; off unless set
listen = "127.0.0.1:4098"                  # must differ from `listen`
token_sha256 = "<sha256 of the admin token>"
```

- A **stream** is one client with its own token and key chain. Streams never share cooldown state unless they list the same key.
- `rate_limit_per_minute` caps a stream's requests per minute. Reiki answers over-limit requests with 429 and `Retry-After` before picking a key, so the upstream and key state are untouched. The counter is in memory and resets on reload.
- `[admin]` opens a separate listener for `GET /status` (JSON) and, with the `metrics` plugin configured, `GET /metrics` (Prometheus text), both behind `Authorization: Bearer <admin token>`. See [docs/monitoring.md](docs/monitoring.md).
- `model_aliases` rewrites the top-level `model` of a JSON body and `drop_params` removes top-level keys, once before the first attempt so every key replay sends the same body. See [docs/providers.md](docs/providers.md#rewriting-the-request-body).
- `http2`, `forward_accept_encoding`, `title_case_headers` and `send_keep_alive` tune the transport per upstream. See [docs/providers.md](docs/providers.md#transport-options).
- `[upstream.X.forward]` hands each request to a Unix socket or a spawned command that speaks HTTP/1.1; `streaming = true` relays the answer as it arrives. See [docs/providers.md](docs/providers.md#forwarding).
- `[upstream.X.fallback]` sends a request to another upstream when none of X's keys is usable, and `passthrough = true` makes X a plain reverse proxy that forwards the client's own credentials. See [docs/providers.md](docs/providers.md).
- `[tunnel]` adds a CONNECT proxy with a destination allowlist; `[tunnel.intercept]` terminates TLS for chosen hosts so plugins and key handling apply to that traffic. See [docs/tunnel.md](docs/tunnel.md).
- `[log]` sends Reiki's own log to a rotating file as well as stderr and sets the level, which SIGHUP reloads. See [docs/troubleshooting.md](docs/troubleshooting.md#logs).
- `[plugins.placeholders]` masks credentials and listed terms before a request leaves and restores them in the response; `[plugins.requestlog]` logs traffic. See [docs/plugins.md](docs/plugins.md).
- `quota_resets` (optional, per key) ends a quota cooldown at the provider's next reset instead of the fixed 5 h. Use `daily HH:MM <zone>` or `monthly D HH:MM <zone>`, where `D` is 1–28 and `<zone>` is `UTC` or a fixed `+HH:MM`/`-HH:MM` offset. It applies only to quota-exhausted failures, and a wait longer than 31 days falls back to the fixed 5 h.
- Keys live in files, never in the config. Keep them mode 0600.
- Generate a token hash with `printf '%s' "$TOKEN" | reiki hash-token`.
- Validate the setup with `reiki check`, then start Reiki with `reiki`.

## Use

Point the client's base URL at `http://127.0.0.1:4097/<upstream>` and send the stream token where the client would normally put its API key. The path prefix picks the upstream, so `/example/chat/completions` goes to `https://api.example.com/v1/chat/completions`. Only the credential header is replaced; every other header passes through.

Clients send the stream token as `Authorization: Bearer`, `x-api-key`, `x-goog-api-key` or a `?key=` query parameter, in that order of preference. Provider-specific setup is in [docs/providers.md](docs/providers.md).

`GET /__reiki/healthz` returns 200 and `ok` when Reiki is up. It needs no token.

## Failover

| Upstream response | Action |
| --- | --- |
| 429 | Cool the key for `Retry-After`, or 60 s |
| 402, or 429 with a quota phrase in the body (`quota`, `usage exceeded`, `insufficient`) | Cool the key for 5 h, or until the key's `quota_resets` instant |
| 400 with an empty-balance body | Cool the key for 5 h, or until the key's `quota_resets` instant |
| 403 or 5xx saying access is disabled | Try the next key for that request only; the key stays in rotation |
| 5xx saying rate limit | Cool the key for `Retry-After`, or 60 s |
| 401, other 403, or 400 with an invalid-key body | Take the key out of rotation until you rotate the key file and reload |
| Other 5xx, connection error, timeout | Retry once on the same key, then cool it for 30 s after the second failure in a row |
| 403 that is a CDN block page | Pass through unchanged |
| Other 4xx | Pass through unchanged |

A failed request replays on the next key as long as no response bytes have reached the client. If every key is out, Reiki returns 503 `reiki_exhausted`, with `Retry-After` when a key will recover.

`Retry-After` is read as delta-seconds or as an HTTP-date (IMF-fixdate, such as `Sun, 06 Nov 1994 08:49:37 GMT`); a cooldown taken from it is capped at 24 h.

Key health persists across restarts in `<config>.state`. See [docs/troubleshooting.md](docs/troubleshooting.md#key-health).

Limits: request bodies up to 8 MiB, 120 s to receive upstream response headers. Reiki reads the config and key files at startup and again on SIGHUP, so send `kill -HUP <pid>` after changing a key file or the config. In-flight requests keep running on the version they started with. A changed `listen` is ignored until you restart Reiki.

See [docs/troubleshooting.md](docs/troubleshooting.md) for status codes and logs, [docs/security.md](docs/security.md) for the security model, and [docs/monitoring.md](docs/monitoring.md) for the optional admin listener.

## Develop

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
