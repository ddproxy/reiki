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
| `intercept` | none | Reads the traffic of chosen hosts; see below. |

## What it does and does not do

- Only `CONNECT` is accepted. Any other request gets 400.
- Names are resolved by Reiki. A name that resolves to a loopback, unspecified, link-local or multicast address is refused with 502, so an allowed name cannot be pointed at Reiki's own admin port or other local services. Private ranges such as `10.0.0.0/8` are allowed.
- Each closed tunnel is logged at `info` with the destination host and port, bytes in each direction and the duration. Nothing inside an opaque tunnel is visible, so there is no request log, no placeholders, no key injection and no failover for it.
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

[[tunnel.intercept.hosts]]
host = "api.example.com"
upstream = "example"      # an [upstream.*] whose base_url is on this host
stream = "laptop"         # the stream whose key chain serves it; omit for a passthrough upstream
```

Create the CA, name-constrained to the hosts it may sign for, and give only `ca.pem` to clients:

```bash
reiki ca-init --dir /etc/reiki/ca --host api.example.com --days 365
```

Clients must trust that certificate: add it to the system store, or point the runtime at it with `SSL_CERT_FILE` or `NODE_EXTRA_CA_CERTS`, whichever the client reads.

- The client uses the upstream's real URL, so no base URL has to change and the client can hold a placeholder instead of a real key. Reiki replaces the credential, as it does for a route.
- Each intercepted host must be allowed by `allow` on port 443 and map to an upstream on that same host.
- Only requests under the upstream's base path are relayed. Others get 403, and a `Host` header that differs from the tunnel destination gets 421.
- When an API spans several path prefixes on one host, list them with `paths = ["/v1", "/other"]`. Matching prefixes (whole segments) are sent to the host exactly as the client sent them, and the upstream's `base_url` then supplies only the host and the credential format.
- Only HTTP/1.1 is offered to the client. Reiki can still use HTTP/2 toward the upstream (`http2 = true`).
- Hosts that are allowed but not listed under `hosts` stay opaque.
- Interception needs a restart to change, like the rest of `[tunnel]`. See [security](security.md) for what to protect.
