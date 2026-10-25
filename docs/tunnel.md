# Tunnel

The `[tunnel]` section starts a second listener that works as a CONNECT-only forward proxy. A client that honours `HTTPS_PROXY` (opencode does, as do curl and most runtimes) asks it to open a TCP connection to `host:port`, and Reiki copies bytes in both directions without reading them. The client's own TLS session, HTTP version and headers reach the destination exactly as the client made them, which no reverse-proxy mode can do.

```toml
[tunnel]
listen = "127.0.0.1:4099"
allow = ["opencode.ai", "*.opencode.ai", "models.dev"]
connect_timeout_secs = 10
```

```bash
HTTPS_PROXY=http://127.0.0.1:4099 NO_PROXY=127.0.0.1,localhost opencode
```

| Key | Default | Meaning |
| --- | --- | --- |
| `listen` | required | Address to listen on. Must differ from `listen` and the admin address. Keep it on loopback: the listener has no authentication. |
| `allow` | required | Destinations a client may open: `host`, `host:port`, `*.host` (subdomains, not the apex) or `*`. A bare host means port 443. A request for anything else gets 403. |
| `connect_timeout_secs` | 10 | How long to wait for the destination to accept the connection. |
| `allow_loopback` | false | Lets the tunnel reach loopback addresses. For tests and local development only. |

## What it does and does not do

- Only `CONNECT` is accepted. Any other request gets 400.
- Names are resolved by Reiki. A name that resolves to a loopback, unspecified, link-local or multicast address is refused with 502, so an allowed name cannot be pointed at Reiki's own admin port or other local services. Private ranges such as `10.0.0.0/8` are allowed.
- Each closed tunnel is logged at `info` with the destination host and port, bytes in each direction and the duration. Nothing inside the connection is visible, so there is no request log, no placeholders, no key injection and no failover for tunnelled traffic. Use the proxy routes for that.
- Changes to `[tunnel]` need a restart; a SIGHUP reload does not rebuild the listener.
