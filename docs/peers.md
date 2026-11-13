# Peers

Reiki instances can share a coarse, read-only summary of their own key health, so an operator or a delegation tool can see which instance has a usable key for an upstream. No request is routed by it: the proxy never reads or acts on peer data.

Everything here is off by default. Without a `[peers]` table, or with `enabled = false`, Reiki opens no extra listener, makes no outbound peer connection, and its admin documents (`/status`, `/v1/openapi.json`) are unchanged.

## Build

Peer support is the `peers` cargo feature, which is in the default feature set like the bundled plugins. It does nothing until the config enables it. A binary built with `--no-default-features` (or without `peers`) refuses a config that has a `[peers]` table, so a table cannot silently do nothing.

## Configure

```toml
[peers]
enabled = false                    # master switch; the table can exist and do nothing
publish = false                    # serve GET /v1/health on `listen`
listen = "127.0.0.1:4099"          # required to publish; its own socket
peer_token_sha256 = "<sha256 of the peer token>"   # required to publish
poll_interval_secs = 30            # reader poll period; data older than 3 periods is unknown
expose_key_labels = false          # publish per-key names and states, not only counts

[peers.ct-4]                       # one table per instance to read
url = "https://10.0.6.153:4099"    # https, or http on a loopback address
token_file = "/etc/reiki/peers/ct-4.token"   # the token ct-4 issued to this instance
ca_file = "/etc/reiki/peers/ct-4-ca.pem"     # required for https; the only CA trusted for ct-4
```

Validation, at startup, on reload and in `reiki check`:

- Settings and peer tables refuse unknown keys. Every sub-table of `[peers]` is a peer, named with letters, digits, `-`, `_` and `.`.
- `poll_interval_secs` is between 1 and 86400.
- With `enabled` and `publish`, `listen` must be an `IP:port` that differs from the client, admin and tunnel listeners, and `peer_token_sha256` is required.
- `peer_token_sha256` must be 64 hex characters and must differ from the admin token hash and from every stream token hash, so a peer token can never act as a stronger credential.
- The peer listener serves plain HTTP, so `listen` must be a loopback address. Reach it from another host through a tunnel you control (WireGuard, `ssh -L`) or a TLS-terminating proxy.
- A peer `url` uses https, or plain http only on a loopback address, and carries no credentials, query or fragment. An https peer needs `ca_file`.

`reiki check` prints a `peers:` line: `peers: disabled`, or the effective settings, for example `peers: enabled, publishing on 127.0.0.1:4099 (key labels hidden), 1 peer(s)[ct-4], poll every 30s`.
