# Security

## Clients and keys

- Clients never see upstream keys. Reiki removes the client's credential headers (`authorization`, `x-api-key`, `x-goog-api-key`) and sets the upstream's header from the key file.
- Stream tokens are stored as SHA-256 hashes and compared in constant time across all streams.
- Upstreams come only from the config. A request cannot choose its own destination, so Reiki is not an open proxy.
- Keys are read from the files named in the config, once at startup. Reiki does not check file permissions, so create them with mode 0600.

## Network

Reiki listens on `listen`, default `127.0.0.1:4097`. It serves plain HTTP and has no TLS. Keep it on loopback, or put a TLS-terminating proxy in front of it before exposing it. Upstream URLs must be `https`, except loopback addresses used in tests.

## Forwarding

Reiki forwards every request header except hop-by-hop headers (`connection`, `keep-alive`, `proxy-*`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `host`, `content-length`, the client credential headers and `accept-encoding`. It asks the upstream for `identity` encoding so error bodies can be classified. Redirects from the upstream are not followed.

## Logging

Key values and stream tokens are not logged. Request and response bodies are not logged, except the first 160 characters of upstream error bodies at `debug` level.

## Limits

- A stream's chain is shared state: two streams that list the same key share its cooldown, and a key rejected with 401 is out of rotation for every stream that lists it.
- Streams are not a security boundary against each other beyond key separation. Run separate instances for mutually untrusted clients.
- There is no per-stream rate limit.
