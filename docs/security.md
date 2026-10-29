# Security

Reiki sits between a client (an agent such as opencode or Claude Code, or an application) and the providers it calls. It isolates each side from what the other should not hold.

It does not make either side trustworthy. A client that can read a secret on its own machine can still send it elsewhere, so masking backs up keeping secrets out of reach and does not replace it.

| Reiki keeps | From | Through |
| --- | --- | --- |
| Upstream API keys | The client | Key files read only by Reiki; credential headers replaced per request |
| Credentials and listed terms in prompts | The provider | The `placeholders` plugin ([placeholders](placeholders.md)) swaps them for placeholders before the request leaves and restores them in the response |
| Arbitrary destinations | The client | Config-only upstreams and the tunnel allowlist |
| A record of what was sent | Nobody; it is for you | The `requestlog` plugin ([requestlog](requestlog.md)), with credentials masked |

## Clients and keys

- Clients never see upstream keys, unless an upstream echoes the key back in an error body that Reiki does not recognise (the exact key and its formatted credential are replaced, other encodings are not). Reiki removes the client's credential headers (`authorization`, `x-api-key`, `x-goog-api-key`) and any `key` query parameter, then sets the upstream's header from the key file.
- The key travels to whoever accepts on an `[upstream.X.forward]` socket or runs as its `command`, in the normal credential header only, with no second copy. A command gets a cleared environment plus `pass_env`. Keep the socket in a directory only the Reiki user and the listener can enter, and set `expect_uid`, which makes Reiki compare the uid of the process that accepted the connection (peer credentials, not the owner of the socket file) and refuse a mismatch. It is optional, so without it whoever binds the path receives the keys. A failed link to the forwarder answers 502 and never cools the key.
- Stream tokens are stored as SHA-256 hashes and compared in constant time across all streams.
- A failed token check is logged with the peer address and path. An address with 10 failures in 60 seconds gets 429 with `Retry-After` until the window passes, and a good login clears its count. The throttle covers the client listener; a refused CONNECT is logged but not counted.
- Upstreams come only from the config. A request cannot choose its own destination, so Reiki is not an open proxy.
- Keys are read from the files named in the config at startup, and again on SIGHUP. Reiki does not check file permissions at startup; `reiki keys --check` reports a key file that the group or other users can read. Create them with mode 0600.

## Network

Reiki listens on `listen`, default `127.0.0.1:4097`. It serves plain HTTP and has no TLS. Keep it on loopback, or put a TLS-terminating proxy in front of it before exposing it. Upstream URLs must be `https`, except loopback addresses used in tests.

## Header handling

Reiki forwards every request header except hop-by-hop headers (`connection`, `keep-alive`, `proxy-*`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `host`, `content-length`, and the client credential headers. `accept-encoding` is replaced with `identity` so error bodies can be classified, unless the upstream sets `forward_accept_encoding` and the client sent one. Redirects from the upstream are not followed.

## Interception

`[tunnel.intercept]` lets Reiki read the traffic of the hosts it lists, so treat the CA as a secret and the clients that trust it as covered by it.

- The CA created by `reiki ca-init` is name-constrained to the hosts you give it. A client that trusts it still rejects a certificate for any other name, so a stolen CA key cannot impersonate other sites.
- The CA key must be readable only by the Reiki service user. Use one CA per machine, never share one across hosts, and install only `ca.pem` in clients.
- Certificates are minted for one exact host and live `leaf_ttl_hours` (default 24). Wildcards and addresses are refused.
- A request is relayed only if its `Host` header matches the tunnel destination and its path lies under the upstream's base path; anything else is refused. This keeps one tunnel from reaching other hosts or other parts of the API.
- Reiki verifies the upstream's certificate on its own TLS connection, as it does for every other route.
- Plugins see decrypted requests and responses on intercepted hosts. Enable `requestlog` bodies there only when you intend to record them.
- Other tunnel destinations stay opaque and are not read.

## Logging

Key values and stream tokens are not logged. Request and response bodies are not logged, except the first 160 characters of upstream error bodies at `debug` level, and whatever the `requestlog` plugin is configured to record, which masks credentials in paths, query parameters, credential headers and JSON keys and values.

## Plugins

Plugins are compiled into the binary and run inside the Reiki process, so a plugin is as trusted as Reiki. A plugin sees request headers (including the client token) and bodies but never an upstream key, which is attached after the hooks run. Only compile in plugins you have read. [Plugins](plugins.md#what-a-plugin-can-see-and-do) covers the hooks, timeouts and panic handling.

## Limits

- A stream's chain is shared state: two streams that list the same key share its cooldown, and a key rejected with 401, or with a 403 or 400 the upstream attributes to the key, is out of rotation for every stream that lists it until it is rotated and Reiki receives SIGHUP.
- Streams are not a security boundary against each other beyond key separation. Run separate instances for mutually untrusted clients.
- A stream's `rate_limit_per_minute`, when set, caps that stream and is enforced before a key is picked.
- The optional admin listener (`[admin]`) is a second socket guarded by its own bearer token, compared by hash in constant time. Keep it on loopback. It exposes key names, upstream names and stream routing, but never key values, token hashes or file paths.

**Key health.** A stream-token holder can influence key health. A 401, or a 403 or 400 the upstream attributes to the key, takes that key out of rotation. To bound the damage, one request disables at most one key: a second key that refuses the same request is tried but not disabled. The invalid-key marker in a 400 body counts only as a real JSON field (`"reason": "API_KEY_INVALID"`), not as text an upstream echoed back.

## Hardening

- The upstream key and the formatted credential are replaced by `[redacted]` in error bodies and headers relayed to a client.
- Account-identifying response headers (`openai-organization`, `openai-project`, `anthropic-organization-id`, `set-cookie`, `set-cookie2`) are not relayed.
- A response body decoded from gzip, deflate, brotli or zstd is cut off with an error past 512 MiB, so a small compressed body cannot expand without bound.
- The `placeholders` plugin keeps credential placeholders masked inside tool-call arguments unless `restore_tool_args` is set.
- Request paths with dot segments or encoded separators are refused.
- The client and admin listeners close a connection that does not finish its headers within 30 seconds, cap open connections (1024 client, 64 admin), and a request body must arrive within the 120-second request timeout (408).
- The admin token must differ from every stream token, and the stream name `passthrough` is reserved.
- The key-health state file is written with mode 0600 through an exclusively created temporary file.
