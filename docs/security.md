# Security

## Clients and keys

- Clients never see upstream keys. Reiki removes the client's credential headers (`authorization`, `x-api-key`, `x-goog-api-key`) and any `key` query parameter, then sets the upstream's header from the key file.
- Stream tokens are stored as SHA-256 hashes and compared in constant time across all streams.
- Upstreams come only from the config. A request cannot choose its own destination, so Reiki is not an open proxy.
- Keys are read from the files named in the config at startup, and again on SIGHUP. Reiki does not check file permissions, so create them with mode 0600.

## Network

Reiki listens on `listen`, default `127.0.0.1:4097`. It serves plain HTTP and has no TLS. Keep it on loopback, or put a TLS-terminating proxy in front of it before exposing it. Upstream URLs must be `https`, except loopback addresses used in tests.

## Forwarding

Reiki forwards every request header except hop-by-hop headers (`connection`, `keep-alive`, `proxy-*`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `host`, `content-length`, the client credential headers and `accept-encoding`. It asks the upstream for `identity` encoding so error bodies can be classified. Redirects from the upstream are not followed.

## Logging

Key values and stream tokens are not logged. Request and response bodies are not logged, except the first 160 characters of upstream error bodies at `debug` level.

## Plugins

Plugins are compiled into the binary and run inside the Reiki process, so a plugin is as trusted as Reiki itself. See [plugins](plugins.md) for the hooks.

- A plugin sees each request's headers, including the client token, and the whole request and response bodies. It never receives an upstream key; the key is attached after the plugin hooks have run, and an `authorization` header set by a plugin is dropped.
- Hooks are synchronous and have no timeout, so a plugin that blocks stalls its own request. A panic in a hook is caught and logged and does not end the request task, but Rust's default panic handler also writes the panic message to stderr, so plugins must not panic on request content. Only compile in plugins you have read.
- Plugin settings are read at startup. A reload checks them and warns when they changed, but does not rebuild plugins.

## Limits

- A stream's chain is shared state: two streams that list the same key share its cooldown, and a key rejected with 401 is out of rotation for every stream that lists it.
- Streams are not a security boundary against each other beyond key separation. Run separate instances for mutually untrusted clients.
- A stream's `rate_limit_per_minute`, when set, caps that stream and is enforced before a key is picked.
- The optional admin listener (`[admin]`) is a second socket guarded by its own bearer token, compared by hash in constant time. Keep it on loopback. It exposes key names, upstream names and stream routing, but never key values, token hashes or file paths.
