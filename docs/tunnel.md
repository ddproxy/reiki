# Tunnel

The `[tunnel]` section starts a second listener that works as a CONNECT forward proxy. A client that honours `HTTPS_PROXY` (most runtimes and tools do) asks it to open a TCP connection to `host:port`. By default Reiki copies bytes in both directions without reading them, so the client's own TLS session, HTTP version and headers reach the destination exactly as the client made them.

```toml
[tunnel]
listen = "127.0.0.1:4099"
allow = ["api.example.com", "*.example.net"]
connect_timeout_secs = 10
```

```bash
HTTPS_PROXY=http://127.0.0.1:4099 NO_PROXY=127.0.0.1,localhost some-client
```

| Key | Default | Meaning |
| --- | --- | --- |
| `listen` | required | Address to listen on. Must differ from `listen` and the admin address. Keep it on loopback or a private interface: the listener has no authentication. |
| `allow` | required | Destinations a client may open: `host`, `host:port`, `*.host` (subdomains, not the apex) or `*`. A bare host means port 443. A request for anything else gets 403. |
| `connect_timeout_secs` | 10 | How long to wait for the destination to accept the connection. |
| `allow_loopback` | false | Lets the tunnel reach loopback addresses. For tests and local development only. |
| `deny_private` | false | Refuses private-range addresses: RFC 1918, `100.64.0.0/10` and IPv6 unique-local. Turn it on with a broad allowlist such as `*` so the tunnel reaches the internet and not the local network. |
| `intercept` | none | Reads the traffic of chosen hosts; see below. |

## What it does and does not do

- Only `CONNECT` is accepted. Any other request gets 400.
- Reiki resolves names itself and refuses with 502 any name that resolves to a loopback, unspecified, link-local or multicast address, so an allowed name cannot reach Reiki's admin port or other local services. Private ranges such as `10.0.0.0/8` are allowed unless `deny_private` is set.
- The filter also refuses `100.100.100.200`, `168.63.129.16` and `fd00:ec2::254`. The host's other interface addresses are not filtered, so avoid a broad allowlist such as `*`.
- Each closed tunnel is logged at `info` with the destination host and port, bytes in each direction and the duration. Nothing inside an opaque tunnel is visible, so there is no request log, no placeholders, no key injection and no failover for it.
- At most 512 tunnels run at once and 64 from one peer address; more get 503. The CONNECT request head must arrive within 10 seconds, and an opaque tunnel is closed after one hour.
- Changes to `[tunnel]` need a restart; a SIGHUP reload does not rebuild the listener.

## Interception

An opaque tunnel cannot be logged, masked or given a key. `[tunnel.intercept]` changes that for the hosts you list: Reiki answers the client's TLS handshake itself with a certificate signed by a CA the client trusts, reads the HTTP request, and sends it through the same path as a normal route, then re-encrypts it on its own TLS session to the real upstream. Plugins, key selection, failover and rate limits all apply.

```toml
[tunnel]
listen = "10.0.0.1:4099"
allow = ["api.example.com"]

[tunnel.intercept]
ca_cert = "/etc/reiki/ca/ca.pem"
ca_key = "/etc/reiki/ca/ca.key"
leaf_ttl_hours = 24
require_auth = true       # CONNECT to a stream host must carry the stream token

[[tunnel.intercept.hosts]]
host = "api.example.com"
upstream = "example"      # an [upstream.*] whose base_url is on this host
stream = "laptop"         # the stream whose key chain serves it; omit for a passthrough upstream
```

Create the CA, name-constrained to the hosts it may sign for, and give only `ca.pem` to clients:

```bash
reiki ca-init --dir /etc/reiki/ca --host api.example.com --days 365
```

A CA is limited to the hosts it was created with, so adding a host means creating a new one with `--replace` and giving clients the new `ca.pem`. `reiki check` prints the days the CA has left, startup logs a warning within 30 days of expiry, and `GET /status` reports `intercept.ca_days_left`.

Clients must trust that certificate: add it to the system store, or point the runtime at it with `SSL_CERT_FILE` or `NODE_EXTRA_CA_CERTS`, whichever the client reads.

- The client uses the upstream's real URL, so no base URL has to change and the client can hold a placeholder instead of a real key. Reiki replaces the credential, as it does for a route.
- Each intercepted host must be allowed by `allow` on port 443 and map to an upstream on that same host.
- Only requests under the upstream's base path are relayed. Others get 403, and a `Host` header that differs from the tunnel destination gets 421.
- Request paths with `.` or `..` segments (also percent-encoded as `%2e`), an encoded `/` (`%2f`), or a backslash are refused with 400 before any path check.
- A CONNECT to an intercepted host served from a stream must carry that stream's token in `Proxy-Authorization`, as a bearer token or as the password of a basic credential, so `HTTPS_PROXY=http://user:TOKEN@host:port` works. Without it the tunnel answers 407 with `Proxy-Authenticate: Basic realm="reiki"`. Hosts on a `passthrough` upstream need none, because they spend no key.
- `require_auth = false` in `[tunnel.intercept]` turns this off (default true). Do that only when every client that can reach the tunnel listener is trusted; Reiki logs a warning at startup when it is off.
- When an API spans several path prefixes on one host, list them with `paths = ["/v1", "/other"]`. Matching prefixes (whole segments) are sent to the host exactly as the client sent them, and the upstream's `base_url` then supplies only the host and the credential format.
- An intercepted request is relayed as the client sent it. The upstream receives the client's header names in their original case and order, its `Accept-Encoding`, and `Connection: keep-alive`, over HTTP/1.1.
- Reiki changes only the credential (replaced in place), `Host` (set to the upstream's) and `Content-Length` (recomputed from the body). It removes hop-by-hop headers: every header named in `Connection` and `Keep-Alive`, `Expect`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade` and `Proxy-*`. The client's `Connection` header is replaced by `keep-alive` or removed.
- Encoded responses (gzip, deflate, brotli, zstd) are decoded so plugins and failover can read them; the client receives plain bytes without `Content-Encoding`. An unknown encoding passes through untouched.
- The per-upstream transport settings (`http2`, `forward_accept_encoding`, `title_case_headers`, `send_keep_alive`) apply to the reverse-proxy routes only, not to intercepted traffic.
- An intercepted host may use an upstream with an `[upstream.X.forward]` table. Reiki still terminates the client's TLS, authenticates the stream and swaps the key; the request then goes to the forwarder instead of Reiki's own TLS client, so the upstream sees the forwarder's handshake. Add `streaming = true` there so event streams are not held until they finish. See [providers](providers.md#forwarding).
- What Reiki cannot reproduce is the client's TLS handshake: the upstream sees Reiki's own TLS client. Only the opaque tunnel keeps the client's handshake, and it cannot swap a key.
- Hosts that are allowed but not listed under `hosts` stay opaque.
- Interception needs a restart to change, like the rest of `[tunnel]`. See [security](security.md) for what to protect.
