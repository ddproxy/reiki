# Peers

Reiki instances can share a coarse, read-only summary of their own key health, so an operator or a monitoring tool can see which instance has a usable key for an upstream. No request is routed by it: the proxy never reads or acts on peer data.

Everything here is off by default. Without a `[peers]` table, or with `enabled = false`, Reiki opens no extra listener, makes no outbound peer connection, and its admin documents (`/status`, `/v1/openapi.json`) are unchanged.

## Build

Peer support is the `peers` cargo feature, in the default set like the bundled plugins. It does nothing until the config enables it, and a binary built with `--no-default-features` (or without `peers`) refuses a `[peers]` table, so a table cannot silently do nothing. Release binaries include it; the release workflow runs each runnable binary through `reiki check` with both disabled configs and requires `peers: disabled`.

## Configure

```toml
[peers]
enabled = false                    # master switch; the table can exist and do nothing
publish = false                    # serve GET /v1/health on `listen`
listen = "127.0.0.1:4099"          # required to publish; its own socket; loopback unless TLS is on
# tls_cert = "/etc/reiki/peers/listener.pem"   # serve HTTPS: PEM chain, leaf first
# tls_key = "/etc/reiki/peers/listener.key"    # its PEM key, mode 0600
# tls_key_permissions = "error"    # or "warn" to load a key group or others can access
peer_token_sha256 = "<sha256 of the peer token>"   # a token named `default`; this or [[peers.tokens]] to publish
# instance = "edge-1"                # published name; "reiki" by default
poll_interval_secs = 30            # reader poll period; data older than 3 periods is unknown
expose_key_labels = false          # publish per-key names and states, not only counts
expose_host_stats = false          # publish load, memory, disk use and active sessions
expose_usage = true                # publish per-upstream attempt counts for the last hour
cooldown_bucket_secs = 300         # published cooldowns round up to a multiple of this; 0 is exact
# stream_interval_secs = 5         # period of `host` events on /v1/health/stream
# max_subscribers = 16             # open streams in total
# max_subscribers_per_peer = 4     # open streams per client address, not counted from loopback
# max_subscribers_per_token = 8    # open streams per peer token
# max_connections_per_addr = 4     # open TCP connections per client address

[[peers.tokens]]                   # more peer tokens, one per reader, each with a name
name = "peer-b"
token_sha256 = "<sha256 of the token issued to peer-b>"

[peers.peer-a]                       # one table per instance to read
url = "https://192.0.2.10:4099"    # https, or http on a loopback address
token_file = "/etc/reiki/peers/peer-a.token"   # the token peer-a issued to this instance
ca_file = "/etc/reiki/peers/peer-a-ca.pem"     # required for https; the only CA trusted for peer-a
```

Validation, at startup, on reload and in `reiki check`:

- Settings and peer tables refuse unknown keys. Every sub-table of `[peers]` is a peer, named with letters, digits, `-`, `_` and `.`.
- `poll_interval_secs` is between 1 and 86400.
- With `enabled` and `publish`, `listen` must be an `IP:port` that differs from the client, admin and tunnel listeners, and `peer_token_sha256` is required.
- Every peer token hash (`peer_token_sha256` and each `[[peers.tokens]]` `token_sha256`) must be 64 hex characters and differ from the admin token hash, every stream token and every other peer token, so a peer token can never act as a stronger credential and a name identifies one token.
- A `[[peers.tokens]]` `name` is lowercase letters, digits and `-`, 1 to 32 bytes, and unique; `peer_token_sha256` takes the name `default`.
- `tls_cert` and `tls_key` go together: both or neither.
- Without them the peer listener serves plain HTTP, so `listen` must be loopback; any other address is refused. Reach it from another host through a tunnel you control (WireGuard, `ssh -L`) or a TLS-terminating proxy, or turn TLS on.
- A peer `url` uses https, or plain http only on a loopback address, and carries no credentials, query or fragment. An https peer needs `ca_file`.

`instance` (optional) sets the published name, with a peer name's character rules; without it the instance is published as `reiki`. The host name is never used, so a host, container or node name does not leave the host unless you set it here. A change to `listen`, or turning publishing on or off, needs a restart and appears in `restart_needed` as `peers.listen`. Tokens and the other settings apply on reload, and a reload that turns publishing off makes the listener answer 404 at once.

## TLS on the peer listener

With `tls_cert` and `tls_key` the peer listener serves HTTPS (rustls, TLS 1.2 and 1.3, HTTP/1.1) and may listen on any address. Any CA may issue the certificate: a private one made for the peers, an internal company CA or a public one. Each reader trusts only the CA in its own `ca_file` for this peer, so a private CA made for the peers is the tightest choice.

- `tls_cert` holds the PEM chain, leaf first. `tls_key` holds its PEM key (PKCS#8, PKCS#1 or SEC1) and must not be readable or writable by group or others, the same rule as the interception CA key: refused by default, loaded with a warning under `tls_key_permissions = "warn"`.
- The certificate must not have expired and the key must match it; otherwise startup, `reiki check` and a reload fail. `reiki check` prints `peers TLS certificate valid for N more day(s)`.
- A reload reads both files again. New contents (a renewed certificate) or new paths swap in without a restart and show in `changed` as `peers.tls`; open connections keep the old certificate. Turning TLS on or off needs a restart and shows in `restart_needed` as `peers.tls`. Both files are watched like key files, so a renewal on disk triggers the reload.

A private CA and a listener certificate with openssl, for a peer reached as `192.0.2.10`:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
  -subj "/CN=reiki peers CA" -keyout peers-ca.key -out peers-ca.pem
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -subj "/CN=peer-a" -keyout listener.key -out listener.csr
openssl x509 -req -in listener.csr -CA peers-ca.pem -CAkey peers-ca.key -CAcreateserial \
  -days 365 -out listener.pem \
  -extfile <(printf 'subjectAltName=IP:192.0.2.10,DNS:peer-a\nextendedKeyUsage=serverAuth')
chmod 600 listener.key peers-ca.key
```

Give `listener.pem` and `listener.key` to the publishing instance, and `peers-ca.pem` to each reader as its `ca_file` for that peer. Keep `peers-ca.key` off the peers.

## Publish: GET /v1/health

With `enabled = true` and `publish = true`, Reiki opens one more listener on `[peers] listen`. It serves exactly one route, `GET /v1/health`; every other path and method answers 404 without a body, whatever the credential.

```bash
printf '%s' "$PEER_TOKEN" | reiki hash-token     # goes in peer_token_sha256
curl -s -H "Authorization: Bearer $PEER_TOKEN" http://127.0.0.1:4099/v1/health
curl -s --cacert peers-ca.pem -H "Authorization: Bearer $PEER_TOKEN" https://192.0.2.10:4099/v1/health
```

```json
{"instance":"peer-a","reiki_version":"0.10.3","observed_at":"2026-11-13T09:30:00Z","upstreams":[
  {"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
   "cooling_combinations":2,
   "soonest_cooldown_seconds":42,"last_status_class":"4xx",
   "requests_1h":120,"limited_1h":3,
   "request_buckets":[0,0,0,0,0,0,0,4,9,18,31,58],
   "tokens_1h":48210,"token_buckets":[0,0,0,0,0,0,0,1900,3600,7200,12400,23110],
   "request_minutes":[3,0,4,2,9,1,0,6,5,7],"token_minutes":[0,0,1900,0,700,300,0,1400,800,2100]}]}
```

- One entry per upstream that uses Reiki's keys; a `passthrough` upstream is left out.
- `soonest_cooldown_seconds` is the shortest wait among the cooling keys, or `null`, rounded up to whole seconds and then to a multiple of `cooldown_bucket_secs` (default 300, at most 86400): a 42 s wait reads as 300 and 301 s as 600, while `0` gives the exact count. Coarse values tell a reader roughly when a key returns without giving away the provider's exact reset. It is relative, so the reader does not depend on this host's clock.
- `last_status_class` is `2xx`, `4xx` or `5xx` for the latest attempt on any of the upstream's keys, or `null` before one or when the attempt got no response.
- `cooling_combinations` counts the (route, model) combinations currently cooling on that upstream's keys, always an integer, `0` when none. It is a count only: no route or model name is ever published, and a key cooling as a whole adds nothing, since its combinations are cleared.
- `reiki_version` is the publisher's version. Releases before it omit it; `reiki top` shows `?` for them.
- `observed_at` is this host's UTC clock when the answer was built, for people and logs.
- `age_seconds` is how old the figures were when the answer was sent. Everything is read as the answer is built, so it is `0`; a reader judges freshness by its own receive time plus this age, with no clock agreement needed. `key` events also carry it.
- With `expose_usage = true` (the default), each upstream also carries the last hour of attempts: `requests_1h` (total attempts), `limited_1h` (attempts that were rate-limited or began a cooldown) and `request_buckets` (12 five-minute counts, oldest first, newest last). It also carries `tokens_1h` and `token_buckets`: the input plus output tokens the provider reported for responses that upstream served, in the same shape. A response without a readable `usage` adds nothing, and a body that is compressed or over the 1 MiB JSON cap is not read (see [monitoring](monitoring.md#get-metrics)). `request_minutes` and `token_minutes` are the same counts for the last ten minutes: ten one-minute values, oldest first, newest last, the newest being the current minute. They saturate at `u32` rather than wrapping. They are counts only, with no key, route or model names, and are kept in memory, so they restart at zero. `expose_usage = false` omits all seven; an older peer that lacks them is read as having no figures.
- With `expose_key_labels = true`, each upstream also carries `keys`: `name`, `state` (`available`, `cooling`, `out_of_rotation`) and `cooldown_seconds`, bucketed the same way. A cooling key also carries `cooldown_class` (`rate_limited`, `quota_exhausted` or `transient`, as in `/status`) when known.
- The answer never holds key values, token hashes, key file paths or stream names; attempt counts are per upstream only, and it is sent with `Cache-Control: no-store`.

### Host stats and sessions

`expose_host_stats = true` (off by default) adds two objects to the answer:

```toml
[peers]
expose_host_stats = true
# session_header = "x-opencode-session"   # client header that names a session
# session_window_secs = 300               # a session is active this long after its last request
```

```json
"host": {"load_average":[0.52,0.58,0.59],"cores":8,
         "memory":{"total_bytes":16384000000,"available_bytes":8192000000,
                   "swap_total_bytes":2048000000,"swap_free_bytes":2048000000},
         "disks":[{"roles":["config","state"],"total_bytes":107374182400,
                   "available_bytes":53687091200,"used_percent":50.0}]},
"sessions": {"window_seconds":300,"active_sessions":3}
```

- `load_average` comes from `/proc/loadavg` and `memory` from `/proc/meminfo`; on a platform without them they are `null`. `cores` is the parallelism available to Reiki.
- `disks` has one entry per filesystem behind the directories Reiki writes to, labelled by role and never path: `config` (the config file), `state` (the key-health state file), `log` (`[log] file`) and `requestlog` (the request log plugin's `file`). Roles on one filesystem share an entry. `used_percent` is computed as `df` does. Figures use `statvfs` from `rustix`, the one dependency the `peers` feature adds. Queries run on Tokio's blocking pool, all at once, with 2 s for the lot; a role whose filesystem errs or does not answer in time (a hung network mount) gets its own entry with `null` figures, and a directory still stuck from an earlier answer is not queried again until that call returns.
- `active_sessions` is client-reported: it counts distinct values of `session_header` seen on authenticated client requests in the last `session_window_secs`, once per client request, so retries, failover and a fallback do not add. A stream-token holder can send many ids and inflate the number, so read it as an activity hint, not a count of real devices. The table stays bounded: values are kept only as salted 64-bit hashes, at most 4096 per stream and 256 streams, least recently seen dropped first; ids are never logged or published. Requests without the header, on a passthrough upstream, or with an id over 256 bytes are not counted.
- A per-stream breakdown (`sessions.streams`, with `stream` and `active_sessions`) appears only when `expose_key_labels` is also on, since stream names identify clients.
- `session_header` must be a lowercase header name and may not be a credential header (`authorization`, `proxy-authorization`, `cookie`, `x-api-key`, `x-goog-api-key`). `session_window_secs` is between 10 and 86400.

### Authentication

- Only `Authorization: Bearer <peer token>` is accepted. The token is hashed and compared in constant time against every peer token hash on each request, so timing does not reveal which matched. Only those hashes count: the admin and stream tokens are refused here, and the peer token is refused on the admin and client listeners because config validation keeps the three apart. `x-api-key` and `?key=` are not read.
- Peer tokens are bearer credentials. Generate each one from at least 32 random bytes (for example `openssl rand -base64 32`) and hash it for the config; `reiki hash-token` enforces a minimum length, not entropy.
- A missing or wrong token gets a bare 401, and each failure is logged with the peer address, never the token. Failures count per source address, an IPv6 address by its /64, towards the same lockout as the other listeners (`[auth]`, default 10 failures in 60 s, then 429 with `Retry-After`, even for a valid token, until the window passes).
  - Loopback is counted too. Peers reaching the listener through `ssh -L`, a WireGuard forward or a local TLS-terminating proxy all share that address, so a stale token behind the tunnel locks out the other peers behind it until the window passes. This is why peer tokens must be high entropy and rotated together.
  - When the tracked-address table is full, the address with the fewest failures, then the oldest, is dropped, so a flood cannot leave a new one untracked.
- A successful login does not clear a failure count on the peer listener, so valid requests between guesses do not reset it; counts expire with the `[auth]` window. The client and admin listeners are unchanged.
- The peer listener keeps its own counts, so failed peer logins never lock an address out of the admin or client listener.
- The matching token's name stays with the request: it counts towards `max_subscribers_per_token` and may appear in Reiki's log lines. The token itself is never logged and the name is never published.
- The listener caps open connections at 32 and closes a connection that does not finish its headers within 30 s.

### What a peer token holder learns

The summary is coarse, but it still tells a reader which upstreams are under rate or quota pressure and when the first key returns; polled over time, that is an activity timeline for the accounts behind this instance. Hand the peer token only to instances and tools inside the operator's trust boundary, never across one. Nothing on this listener can change state.

The route is listed in the admin `GET /v1/openapi.json` while publishing, with the peer listener as its `servers` entry and a separate `peerAuth` security scheme. Without publishing the document is unchanged.

## Push: GET /v1/health/stream

The same listener and token also serve `GET /v1/health/stream`, a Server-Sent Events stream, so a reader need not poll:

```bash
curl -N -H "Authorization: Bearer $PEER_TOKEN" http://127.0.0.1:4099/v1/health/stream
```

```text
event: snapshot
data: {"instance":"peer-a","observed_at":"...","upstreams":[...]}

event: key
data: {"observed_at":"...","upstream":{"name":"zen","healthy_keys":0,"cooling_keys":2,...}}

: heartbeat
```

- `snapshot` comes first on every connection and holds the full `/v1/health` document; it is sent again if the set of upstreams changes on a reload.
- `key` carries one upstream entry whenever its key counts, its `last_status_class` or (with labels) a key's state changes. A cooldown counting down is not a change.
- With `expose_host_stats`, `host` comes every `stream_interval_secs` (default 5) and `sessions` whenever the session counts change.
- A `: heartbeat` comment comes every 15 seconds, so a reader can tell a quiet stream from a dead one.
- Changes are checked once a second, only while at least one stream is open and publishing is on: the checker starts with the first subscriber, stops once the last leaves or a reload stops publishing, and reads no key health, `/proc` or disk figures in between.
- Changes are fed through one bounded broadcast. A subscriber that stops reading, or falls behind, is dropped once 32 frames are queued for it, not buffered without limit.
- `max_subscribers` (default 16, at most 256) caps open streams in total, `max_subscribers_per_peer` (default 4) caps them per client address (not applied to loopback connections, which all share one address; the token cap tells those apart) and `max_subscribers_per_token` (default 8, at most 256) per peer token; past the first the answer is 503, past the others 429, with `Retry-After`. A closed connection frees its place. The listener accepts `max_subscribers` plus 16 connections, and each remote address may hold at most `max_connections_per_addr` of them (default 4, between 1 and 256); a connection past that is dropped at accept. A 401 or 429 answer carries `Connection: close`.
- The stream carries the same fields as `/v1/health` and nothing else: no bodies, no key labels unless `expose_key_labels`, no stream names. A reload that stops publishing ends every open stream; one that removes a peer token or changes its hash ends the streams opened with that token, within a second, and leaves the others open.

## Read: reiki peers

`reiki peers` fetches `/v1/health` from every `[peers.<name>]` in the config and prints one row per peer upstream:

```bash
reiki peers                 # table
reiki peers --json          # {"peers":[{"name","state","error","instance","age_seconds","upstreams"}]}
```

```text
PEER  INSTANCE  UPSTREAM  STATE    HEALTHY  COOLING  DISABLED  WAIT  AGE
peer-a  peer-a      zen       cooling  0        1        0         42s   3s
peer-b  -         -         unknown  -        -        -         -     -
```

- It needs `[peers] enabled = true` and at least one peer. Nothing else in the config is used, and the proxy itself never reads peer data.
- Each peer gets its own HTTP client. The built-in and system roots are off, so only certificates in that peer's `ca_file` are trusted for it (put two in the file to overlap a CA rotation). The provider clients are never touched, so a peer CA can never vouch for a provider. Proxies from the environment are ignored.
- Only the token from `token_file` is sent, as `Authorization: Bearer`. A token file its group or others can read is refused before any request; keep it mode 0600.
- Redirects are not followed: a 3xx is a failure, so the token never goes to a second host. Each fetch has 5 s to finish, connect included, and a body over 64 KiB is refused rather than cut.
- A peer shows as `unknown`, with no counts, when it cannot be reached, fails the TLS check, refuses the token, answers anything but 200, sends a body that is too big or not a health document, or sends data older than three `poll_interval_secs`. The age is the peer's `age_seconds` plus the time since this host received it, so a peer whose clock is off still reads as fresh. Only a peer that sends no `age_seconds` (an older release) is judged by its `observed_at` against this host's clock, and then data that far in the future is unknown too. `observed_at` may carry fractional seconds, a lowercase `t` or `z`, or a numeric offset such as `+01:00`. The reason goes to stderr. Unknown never means healthy.
- `STATE` is `ok` while a key is usable, else `cooling`, `disabled` or `no_keys`. `WAIT` is `soonest_cooldown_seconds` and `AGE` the age of the data.
- Exit status: 0 when at least one peer gave fresh data, 1 when none did, 2 on a config problem.

`reiki peers --watch` follows every peer's `/v1/health/stream` instead and prints the table again (or, with `--json`, one JSON document per line) whenever something changes. Each stream uses the same pinned client and token rules, with a 64 KiB cap per event. When a stream cannot be opened, ends, or sends nothing for 40 s, it polls `/v1/health` once, waits `poll_interval_secs` and tries the stream again. `AGE` is the age the peer last reported plus the time since the last frame from it, heartbeats included, measured on this host's clock; a peer with nothing for three poll intervals turns `unknown`. It runs until interrupted.

## Check

`reiki check` prints a `peers:` line: `peers: disabled`, or the effective settings, for example `peers: enabled, publishing on 127.0.0.1:4099 (key labels hidden), 1 peer(s)[peer-a], poll every 30s`. With more than one peer token it names them, as `2 token(s)[default, peer-b]`, after the listener.
