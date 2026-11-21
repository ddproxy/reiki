# Publishing peer health

A Reiki instance publishes a coarse, read-only summary of its own key health on a separate peer listener. This page covers that listener, its tokens, TLS, the health document and the event stream. [Peers](peers.md) covers reading those documents with `reiki peers` and `reiki top`; the full [field schema](peers.md#health-document-schema) lives there too.

Everything here is off by default. With no `[peers]` table, or with `enabled = false`, Reiki opens no extra listener.

## Build

Peer support is the `peers` cargo feature, in the default set like the bundled plugins. A binary built with `--no-default-features` (or without `peers`) refuses a `[peers]` table. Release binaries include it; the release workflow runs each runnable binary through `reiki check` with both disabled configs and requires `peers: disabled`. The `top` feature implies `peers`; see [reiki top](top.md).

## Configure

```toml
[peers]
enabled = true                         # master switch
publish = true                         # serve GET /v1/health on `listen`
listen = "127.0.0.1:4099"              # its own socket; loopback unless TLS is on
# tls_cert = "/etc/reiki/peers/listener.pem"
# tls_key = "/etc/reiki/peers/listener.key"
# tls_key_permissions = "error"        # or "warn" for a key readable by group or others
peer_token_sha256 = "<sha256 of the peer token>"   # token named `default`; or use [[peers.tokens]]
# instance = "edge-1"                  # published name; "reiki" by default
poll_interval_secs = 30                # reader poll period; data older than 3 periods is unknown
expose_key_labels = false              # publish per-key names and states, not only counts
expose_host_stats = false              # publish load, memory, disk use and active sessions
expose_usage = true                    # publish per-upstream attempt counts for the last hour
cooldown_bucket_secs = 300             # published cooldowns round up to a multiple of this; 0 is exact
# stream_interval_secs = 5             # period of `host` events on /v1/health/stream
# max_subscribers = 16                 # open streams in total
# max_subscribers_per_peer = 4         # open streams per client address, not counted from loopback
# max_subscribers_per_token = 8        # open streams per peer token
# max_connections_per_addr = 4         # open TCP connections per client address

[[peers.tokens]]                       # more peer tokens, one per reader, each with a name
name = "peer-b"
token_sha256 = "<sha256 of the token issued to peer-b>"
```

The same table can also list readers under `[peers.NAME]`; those entries belong to the [reading side](peers.md#read-with-reiki-peers). Validation at startup, on reload and in `reiki check`:

- Settings and peer tables refuse unknown keys. Every sub-table of `[peers]` is a peer name made of letters, digits, `-`, `_` and `.`.
- `poll_interval_secs` is between 1 and 86400.
- With `enabled` and `publish`, `listen` must be an `IP:port` that differs from the client, admin and tunnel listeners, and `peer_token_sha256` is required.
- Every peer token hash (`peer_token_sha256` and each `[[peers.tokens]]` `token_sha256`) is 64 hex characters and differs from the admin token hash, every stream token and every other peer token. A `[[peers.tokens]]` `name` is lowercase letters, digits and `-`, 1 to 32 bytes, unique; `peer_token_sha256` takes the name `default`.
- `tls_cert` and `tls_key` go together. Without them the listener serves plain HTTP, so `listen` must be loopback; reach it from another host through a tunnel you control or a TLS-terminating proxy.

`instance` sets the published name with a peer name's character rules; the host name is never published. Changing `listen`, turning publishing on or off, or toggling TLS needs a restart and appears in `restart_needed` as `peers.listen` or `peers.tls`. Tokens and other settings apply on reload; a reload that stops publishing makes the listener answer 404 at once. The peer listener keeps its own failed-login counts, separate from the admin and client listeners.

`reiki check` prints `peers: enabled, publishing on 127.0.0.1:4099 (key labels hidden), 1 peer(s)[peer-a], poll every 30s`. With more than one token it adds `2 token(s)[default, peer-b]` after the listener; with publishing off it prints `peers: disabled` or the reader settings.

## TLS on the peer listener

With `tls_cert` and `tls_key` the listener serves HTTPS (rustls, TLS 1.2 and 1.3, HTTP/1.1) on any address. Any CA may issue the certificate: a private one made for the peers, an internal company CA or a public one. Each reader trusts only the CA in its own `ca_file` for this peer.

- `tls_cert` holds the PEM chain, leaf first. `tls_key` holds its PEM key (PKCS#8, PKCS#1 or SEC1) and stays readable and writable only by its owner: refused by default, loaded with a warning under `tls_key_permissions = "warn"`.
- The certificate must be unexpired and the key must match it; otherwise startup, `reiki check` and a reload fail. `reiki check` prints `peers TLS certificate valid for N more day(s)`.
- A reload reads both files again. New contents or new paths swap in without a restart and show in `changed` as `peers.tls`; open connections keep the old certificate. Both files are watched like key files, so a renewal on disk triggers the reload. Turning TLS on or off needs a restart. Reiki tracks the running listener's scheme on every start, so a later reload still reports the switch as needing a restart even after the listener has been restarted.

A private CA and a listener certificate for a peer reached as `192.0.2.10`:

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

Give `listener.pem` and `listener.key` to the publishing instance, and `peers-ca.pem` to each reader as its `ca_file`. Keep `peers-ca.key` off the peers.

## Tokens and authentication

Generate each peer token from at least 32 random bytes and hash it for the config:

```bash
openssl rand -base64 32 | reiki hash-token
printf '%s' "$PEER_TOKEN" | reiki hash-token     # for peer_token_sha256 or [[peers.tokens]]
```

`reiki hash-token` enforces a minimum length, not entropy. Only `Authorization: Bearer <peer token>` is accepted. The token is hashed and compared in constant time against every peer token hash on each request, so timing gives away no matching entry. Admin and stream tokens are refused here, and a peer token is refused on the admin and client listeners because validation keeps the three apart. `x-api-key` and `?key=` are ignored.

A missing or wrong token gets a bare `401`, logged with the peer address and never the token. Failures count per source address, an IPv6 address by its /64, towards `[auth]`'s lockout: the default is 10 failures in 60 seconds, then `429` with `Retry-After` even for a valid token until the window passes. Loopback counts too, so peers reaching the listener through `ssh -L`, WireGuard or a local TLS proxy share one tracked address; rotate those tokens together. When the address table is full, the address with the fewest failures, then the oldest, is dropped. A successful login leaves the failure count in place; counts expire with the window. The matching token's name stays with the request, counts towards `max_subscribers_per_token` and may appear in the log; the token itself is never logged or published.

## Publish: GET /v1/health

With `enabled = true` and `publish = true`, Reiki opens one more listener on `[peers] listen`. It serves `GET /v1/health`; every other path and method answers 404 without a body, whatever the credential.

```bash
curl -s -H "Authorization: Bearer $PEER_TOKEN" http://127.0.0.1:4099/v1/health
curl -s --cacert peers-ca.pem -H "Authorization: Bearer $PEER_TOKEN" https://192.0.2.10:4099/v1/health
```

```json
{"instance":"peer-a","reiki_version":"0.10.6","observed_at":"2026-11-13T09:30:00Z","age_seconds":0,"upstreams":[
  {"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
   "cooling_combinations":2,
   "soonest_cooldown_seconds":300,"last_status_class":"4xx",
   "requests_1h":120,"limited_1h":3,
   "request_buckets":[0,0,0,0,0,0,0,4,9,18,31,58],
   "tokens_1h":48210,"token_buckets":[0,0,0,0,0,0,0,1900,3600,7200,12400,23110],
   "request_minutes":[3,0,4,2,9,1,0,6,5,7],"token_minutes":[0,0,1900,0,700,300,0,1400,800,2100]}],
 "failures":{"rate_limited":7,"quota_exhausted":1}}
```

- One `upstreams` entry per upstream that uses Reiki's keys, in config order; a `passthrough` upstream is left out.
- `soonest_cooldown_seconds` rounds up to whole seconds and then to a multiple of `cooldown_bucket_secs` (default 300, at most 86400): a 42 s wait reads as 300, 301 s as 600, and `0` stays exact. It is relative, so readers need no agreement with this host's clock.
- `last_status_class` is `2xx`, `4xx` or `5xx` for the latest attempt on any of the upstream's keys, or `null` before one or when it got no response.
- `cooling_combinations` counts `(route, model)` combinations currently cooling on that upstream's keys. Whole-key cooldowns clear their combinations.
- `reiki_version` is the publisher's version; older publishers omit it and `reiki top` shows `?`.
- `observed_at` is this host's UTC clock when the answer was built. `age_seconds` is how old the figures were when sent; every figure is read as the answer is built, so it is `0`.
- With `expose_usage = true` (the default), each upstream also carries `requests_1h`, `limited_1h`, `request_buckets` (12 five-minute counts, oldest first), `tokens_1h`, `token_buckets`, `request_minutes` and `token_minutes` (10 one-minute values ending with the current minute). Bodies that are compressed or over the 1 MiB JSON cap add nothing; the same counters drive the gauges in [monitoring](monitoring.md#get-metrics). They saturate at `u32` and restart at zero.
- With `expose_key_labels = true`, each upstream also carries `keys`: `name`, `state` (`available`, `cooling`, `out_of_rotation`) and `cooldown_seconds`, bucketed the same way. A cooling key also carries `cooldown_class` (`rate_limited`, `quota_exhausted` or `transient`) when known.
- `failures` holds per-class counts over the same bounded ring as `GET /failures`: `rate_limited`, `quota_exhausted`, `access_disabled`, `unauthorized` and `transient` appear only when at least one such failure was recorded. Bodies, key names, route names and model names stay out.
- The answer carries no key values, token hashes, key file paths or stream names, except `sessions.streams` when `expose_key_labels` is on. It is sent with `Cache-Control: no-store`. The table of every field is in [peers](peers.md#health-document-schema).

### Host stats and sessions

`expose_host_stats = true` adds `host` and `sessions` to the answer:

```toml
[peers]
expose_host_stats = true
# session_header = "x-opencode-session"
# session_window_secs = 300
```

```json
"host": {"load_average":[0.52,0.58,0.59],"cores":8,
         "memory":{"total_bytes":16384000000,"available_bytes":8192000000,
                   "swap_total_bytes":2048000000,"swap_free_bytes":2048000000},
         "disks":[{"roles":["config","state"],"total_bytes":107374182400,
                   "available_bytes":53687091200,"used_percent":50.0}]},
"sessions": {"window_seconds":300,"active_sessions":3}
```

- `load_average` comes from `/proc/loadavg` and `memory` from `/proc/meminfo`; a platform without them gives `null`. `cores` is the parallelism available to Reiki.
- `disks` holds one entry per filesystem behind the directories Reiki writes to, labelled by role and never by path: `config`, `state`, `log` and `requestlog`. Roles on one filesystem share an entry. `used_percent` matches `df`. Queries run on Tokio's blocking pool with 2 seconds for the lot; a role whose filesystem errs or times out gets its own entry with `null` figures, and a directory still stuck from an earlier answer is left alone until that call returns.
- `active_sessions` counts distinct values of `session_header` seen on authenticated client requests in `session_window_secs`, once per client request, so retries, failover and fallback add nothing. A token holder can send many ids and inflate it, so read it as an activity hint. Values live as salted 64-bit hashes, at most 4096 per stream and 256 streams, least recently seen dropped first; ids are never logged or published. Requests without the header, on a passthrough upstream, or with an id over 256 bytes add nothing.
- A per-stream breakdown (`sessions.streams`, with `stream` and `active_sessions`) appears only with `expose_key_labels = true`, because stream names identify clients. `session_header` must be a lowercase header name outside the credential headers (`authorization`, `proxy-authorization`, `cookie`, `x-api-key`, `x-goog-api-key`); `session_window_secs` is between 10 and 86400.

### What a token holder learns

The summary is coarse, but it still reveals which upstreams are under rate or quota pressure and when the first key returns. Polled over time, that is an activity timeline for the accounts behind this instance. Give the peer token only to instances and tools inside the operator's trust boundary. Nothing on this listener changes state.

While publishing, the admin `GET /v1/openapi.json` lists the peer routes with the peer listener as their `servers` entry and a separate `peerAuth` security scheme; see [monitoring](monitoring.md#get-v1openapijson).

## Push: GET /v1/health/stream

The same listener and token serve `GET /v1/health/stream`, a Server-Sent Events stream:

```bash
curl -N -H "Authorization: Bearer $PEER_TOKEN" http://127.0.0.1:4099/v1/health/stream
```

```text
event: snapshot
data: {"instance":"peer-a","observed_at":"...","upstreams":[...],"failures":{}}

event: key
data: {"observed_at":"...","upstream":{"name":"zen","healthy_keys":0,"cooling_keys":2,...}}

: heartbeat
```

- `snapshot` comes first on every connection and carries the full `/v1/health` document; it is sent again when the set of upstreams changes on a reload.
- `key` carries one upstream entry whenever its key counts, `last_status_class` or (with labels) a key state changes. A cooldown counting down is not a change.
- With `expose_host_stats`, `host` comes every `stream_interval_secs` (default 5) and `sessions` whenever session counts change.
- A `: heartbeat` comment comes every 15 seconds, so a reader can tell a quiet stream from a dead one.
- Changes are checked once a second, only while at least one stream is open and publishing is on. Changes go through one bounded broadcast; a subscriber that falls behind is dropped after 32 queued frames.
- `max_subscribers` (default 16, at most 256) caps open streams; `max_subscribers_per_peer` (default 4) caps them per client address, with loopback exempt because all loopback connections share one address; `max_subscribers_per_token` (default 8, at most 256) caps them per token. Past the first limit the answer is 503, past the others 429 with `Retry-After`; a closed connection frees its place. The listener accepts `max_subscribers` plus 16 connections, with at most `max_connections_per_addr` per remote address (default 4, between 1 and 256). A 401 or 429 answer carries `Connection: close`.
- The stream carries the same fields as `/v1/health`: bodies and key labels stay out unless `expose_key_labels` is on, and stream names appear only in `sessions.streams` under that same switch. A reload that stops publishing ends every open stream; one that removes a peer token or changes its hash ends the streams opened with that token within a second.
