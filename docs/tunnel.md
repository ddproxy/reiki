# Tunnel

`[tunnel]` starts a second listener that works as a CONNECT forward proxy. A client that honours `HTTPS_PROXY` asks it to open a TCP connection to `host:port`. By default Reiki copies bytes in both directions without reading them, so the client's TLS session, HTTP version and headers reach the destination unchanged.

```toml
[tunnel]
listen = "127.0.0.1:4098"
allow = ["api.example.com", "*.example.net"]
connect_timeout_secs = 10
```

```sh
HTTPS_PROXY=http://127.0.0.1:4098 NO_PROXY=127.0.0.1,localhost some-client
```

| Key | Default | Meaning |
| --- | --- | --- |
| `listen` | required | Must differ from the client and admin addresses. The listener has no authentication; keep it on loopback or a private interface. |
| `allow` | required | Permitted destinations: `host`, `host:port`, `*.host` (subdomains, not the apex) or `*`. A bare host means port 443; anything else gets 403. |
| `connect_timeout_secs` | 10 | Wait for the destination to accept the connection. |
| `allow_loopback` | false | Permit loopback destinations. For tests and local development only. |
| `deny_private` | false | Refuse RFC 1918, `100.64.0.0/10` and IPv6 unique-local addresses. Use with a broad `allow` list so the tunnel reaches the internet, not the local network. |
| `close_idle_on_stop` | true | On stop, close tunnels idle for at least one second at once; active tunnels get the configured drain. |
| `intercept` | none | Read traffic for chosen hosts; see [interception](#interception). |

## What it does and does not do

- Only `CONNECT` is accepted; any other request gets 400.
- Reiki resolves names itself and refuses with 502 any name resolving to a loopback, unspecified, link-local or multicast address. It also refuses `100.100.100.200`, `168.63.129.16` and `fd00:ec2::254`. Private ranges are permitted unless `deny_private` is set; other interface addresses are not filtered, so avoid `allow = ["*"]`.
- Each closed tunnel is logged at `info` with destination, bytes in each direction and duration. Nothing inside an opaque tunnel is visible: no request log, placeholders, key injection or failover.
- At most 512 tunnels run at once and 64 from one peer address; more get 503. A CONNECT head must arrive within 10 seconds, and an opaque tunnel is closed after one hour.
- A clean stop closes idle tunnels immediately with `close_idle_on_stop = true`; moving tunnels get the full drain, bounded by `[service] shutdown_timeout_secs`.
- Reload changes `allow`, `deny_private`, `allow_loopback`, `connect_timeout_secs` and `close_idle_on_stop` without a restart; open tunnels keep their original policy. A new `listen` address needs a restart. See [reload](troubleshooting.md#reload).

## Interception

An opaque tunnel cannot be logged, masked or given a key. `[tunnel.intercept]` changes that for listed hosts: Reiki answers the client's TLS handshake with a certificate signed by a CA the client trusts, reads the HTTP request, sends it through the normal route with plugins, key selection, failover and limits, then re-encrypts it on its own TLS session to the real upstream.

```toml
[tunnel]
listen = "192.0.2.1:4098"
allow = ["api.example.com"]

[tunnel.intercept]
ca_cert = "/etc/reiki/ca/ca.pem"
ca_key = "/etc/reiki/ca/ca.key"
leaf_ttl_hours = 24
require_auth = true
ca_key_permissions = "error"   # or "warn"

[[tunnel.intercept.hosts]]
host = "api.example.com"
upstream = "example"      # an [upstream.*] whose base_url is on this host
stream = "client-a"         # omit for a passthrough upstream
```

Create the CA, name-constrained to the hosts it may sign for, and give clients only `ca.pem`:

```sh
reiki ca-init --dir /etc/reiki/ca --host api.example.com --days 365
```

Adding a host means creating a new constrained CA with `--replace` and distributing the new `ca.pem`. `reiki check` reports days left, startup warns within 30 days of expiry, and `GET /v1/status` reports `intercept.ca_days_left`. Clients must trust the CA through their system store or `SSL_CERT_FILE` / `NODE_EXTRA_CA_CERTS`.

- Each intercepted host must be allowed on port 443 and map to an upstream on that same host. Only requests under the upstream's base path are relayed; others get 403 and a mismatched `Host` gets 421.
- Paths with `.` or `..` segments, encoded separators (`%2f`) or backslashes are refused with 400. Percent decoding runs up to eight times, so `%252e%252e` is refused too, as are malformed escapes.
- A CONNECT to an intercepted host served from a stream must carry that stream's token in `Proxy-Authorization`, as a bearer token or the password of a basic credential. Without it the tunnel answers 407 with `Proxy-Authenticate: Basic realm="reiki"`. Bad tokens count towards `[auth]` backoff; after `max_failures` the tunnel answers 429 with `Retry-After`. Passthrough hosts need no token. `require_auth = false` turns this off and warns at startup.
- When an API spans several path prefixes on one host, list them with `paths = ["/v1", "/other"]`. Whole-segment prefix matches are relayed as sent; `base_url` then supplies only host and credential format.
- The upstream receives the client's header names in their original case and order, its `Accept-Encoding` and `Connection: keep-alive` over HTTP/1.1. Reiki changes only the credential, `Host` and `Content-Length`, and removes hop-by-hop headers named in `Connection` and `Keep-Alive` plus `Expect`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade` and `Proxy-*`.
- Encoded responses (gzip, deflate, brotli, zstd) are decoded so plugins and failover can read them; the client receives plain bytes without `Content-Encoding`. An encoding Reiki did not decode is refused with 502 when a key is in play, and relayed as-is with no key.
- Per-upstream transport settings (`http2`, `forward_accept_encoding`, `title_case_headers`, `send_keep_alive`) apply to reverse-proxy routes only. An intercepted host may use `[upstream.X.forward]`; add `streaming = true` there so event streams are not buffered. The upstream sees Reiki's TLS client, not the client's handshake. See [forwarding](providers.md#forwarding).
- Hosts that are allowed but not listed under `hosts` stay opaque.
- Reload changes intercept hosts, `require_auth`, `leaf_ttl_hours` and a renewed CA; open connections keep their host entry and certificate. A CA that fails validation refuses the whole reload. Adding or removing `[tunnel.intercept]` needs a restart. See [reload](troubleshooting.md#reload) and [security](security.md#interception).

## Trust boundary

A plain CONNECT to a host on the `allow` list needs no credential: the list is the only gate, and `*` is legal. Only intercepted hosts check the stream token. Put the listener on a network only trusted clients can reach, keep `deny_private = true`, and keep `allow` as short as the clients need. `deny_private` covers RFC 1918, shared address space, benchmarking and IETF protocol blocks, unique-local IPv6, and IPv4 forms embedded in IPv4-mapped, NAT64, 6to4 and Teredo addresses. See [what Reiki does not protect against](security.md#what-reiki-does-not-protect-against).
