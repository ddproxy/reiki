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
# instance = "ct-1"                # published name; the host name by default
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

`instance` (optional) sets the name published as `instance`, with the same character rules as a peer name; without it the host name is used. A change to `listen`, or turning publishing on or off, needs a restart and shows in a reload's `restart_needed` as `peers.listen`. The token hash and the other settings apply on reload, and a reload that turns publishing off makes the listener answer 404 at once.

## Publish: GET /v1/health

With `enabled = true` and `publish = true`, Reiki opens one more listener on `[peers] listen`. It serves exactly one route, `GET /v1/health`; every other path and method answers 404 without a body, whatever the credential.

```bash
printf '%s' "$PEER_TOKEN" | reiki hash-token     # goes in peer_token_sha256
curl -s -H "Authorization: Bearer $PEER_TOKEN" http://127.0.0.1:4099/v1/health
```

```json
{"instance":"ct-4","observed_at":"2026-11-13T09:30:00Z","upstreams":[
  {"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
   "soonest_cooldown_seconds":42,"last_status_class":"4xx"}]}
```

- One entry per upstream that uses Reiki's keys; a `passthrough` upstream is left out.
- `soonest_cooldown_seconds` is the shortest wait among the cooling keys, rounded up, or `null`. It is relative, so a reader does not depend on this host's clock.
- `last_status_class` is `2xx`, `4xx` or `5xx` for the latest attempt on any of the upstream's keys, or `null` before one or when the attempt got no response.
- `observed_at` is this host's UTC clock when the answer was built.
- With `expose_key_labels = true`, each upstream also carries `keys`: `name`, `state` (`available`, `cooling`, `out_of_rotation`) and `cooldown_seconds`.
- The answer never holds key values, token hashes, key file paths, stream names or request counts, and is sent with `Cache-Control: no-store`.

### Authentication

- Only `Authorization: Bearer <peer token>` is accepted. The token is hashed and compared in constant time against `peer_token_sha256`, and only that hash: the admin token and the stream tokens are refused here, and the peer token is refused on the admin and client listeners because config validation keeps the three apart. `x-api-key` and `?key=` are not read.
- A missing or wrong token gets a bare 401. Each failure is logged with the peer address and never the token, and counts towards the same per-address lockout as the other listeners (`[auth]`, default 10 failures in 60 s, then 429 with `Retry-After`). The peer listener keeps its own counts, so failed peer logins never lock an address out of the admin or client listener. When the table of tracked addresses is full, the address with the fewest failures, then the oldest, is dropped, so a flood of addresses cannot leave a new one untracked.
- The listener caps open connections at 32 and closes a connection that does not finish its headers within 30 s.

### What a peer token holder learns

The summary is coarse, but it still tells a reader which of this instance's upstreams are under rate or quota pressure right now and when the first key comes back. Polled over time, that is an activity timeline for the accounts behind this instance. Hand the peer token only to instances and tools inside the same operator's trust boundary, and never publish across one. Nothing on this listener can change state.

The route is listed in the admin `GET /v1/openapi.json` while publishing, with the peer listener as its `servers` entry and a separate `peerAuth` security scheme. Without publishing the document is unchanged.

## Check

`reiki check` prints a `peers:` line: `peers: disabled`, or the effective settings, for example `peers: enabled, publishing on 127.0.0.1:4099 (key labels hidden), 1 peer(s)[ct-4], poll every 30s`.
