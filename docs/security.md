# Security

Reiki sits between a client (an agent or an application) and the providers it calls. It keeps upstream credentials with Reiki, applies configured controls to traffic that passes through it, and separates listeners by credential. Controls outside that path are listed in [what Reiki does not protect against](#what-reiki-does-not-protect-against).

| Reiki keeps | From | Through |
| --- | --- | --- |
| Upstream keys | The client | Key files read by Reiki; credential headers replaced per request |
| Credentials and listed terms in prompts | The provider | `placeholders` swaps them out before the request leaves and restores them in the response ([plugins](plugins.md#placeholders)) |
| Arbitrary destinations | The client | Config-only upstreams and the tunnel allow-list ([tunnel](tunnel.md#trust-boundary)) |
| A record of what was sent | Nobody; it is for the operator | `requestlog` records summaries by default ([requestlog](requestlog.md)) |
| Dynamic stream management | A holder of the dedicated stream-admin token | A separate credential on the admin listener, strict JSON and a hash-chained audit log ([dynamic streams](dynamic-streams.md)) |

## Clients and keys

- Client credential headers (`authorization`, `x-api-key`, `x-goog-api-key`) and the `key` query parameter are removed on key-backed routes. Reiki sets the upstream's header from the key file. A `passthrough` upstream receives the client's own credentials and query string unchanged. See [keys](keys.md#stream-tokens).
- Upstreams come only from the config, so a request cannot choose its own destination.
- A key travels to whoever accepts on an `[upstream.X.forward]` socket or runs as its `command`, only in the normal credential header. A command gets an environment cleared except `PATH`, `HOME`, `LANG`, `LC_ALL` and the names in `pass_env`. Keep the socket in a directory only the Reiki user and listener can enter and set `expect_uid`: Reiki then compares the uid of the accepting process (peer credentials, not the socket file's owner) and refuses a mismatch. Without `expect_uid`, whoever binds the path receives the keys; a parent directory that is group- or other-writable without the sticky bit is refused at load. A failed link to the forwarder answers 502 and never cools the key. See [forwarding](providers.md#forwarding).
- Key values are trimmed and must be 8 to 4096 bytes; a file outside that range is refused at load. Key files are read at startup and again on SIGHUP or reload, on the same handle whose mode is checked. With the default `strict_key_permissions = true`, group or other read/write bits are a load error; `reiki keys --check` reports the same condition as a warning and exits `1`. Create key files mode `0600` in a private directory. See [key files](keys.md#key-files).
- Stream tokens are stored as SHA-256 hashes and compared in constant time across all streams. `reiki hash-token` refuses a token shorter than 16 characters; a `token_sha256` is verified only as 64 hex characters. Use at least 16 random characters.
- The interception CA key and peer listener TLS key are checked for group or other access when loaded (`ca_key_permissions` and `[peers].tls_key_permissions`, `error` or `warn`), at startup, `reiki check` and a reload that reads them. Keep both private to the Reiki user.
- A client can send a stream token as `Authorization: Bearer <token>`, `x-api-key`, `x-goog-api-key` or `?key=`; the first form found wins. The client listener serves plain HTTP and has no client-certificate identity of its own.

### Key redaction

Keys are never sent to a client on purpose; this is the safeguard for an upstream that repeats a credential. Request-log masking is a separate mechanism ([request log](requestlog.md#what-is-kept-out-of-the-log)).

| Where | Replacement | Length |
| --- | --- | --- |
| Headers relayed to the client, and upstream error bodies before they are cut to 64 KiB | `[redacted]` | Changes |
| Successful response bodies, streamed | Same-length `[***…]` marker | Kept |
| Failure-ring snippets (cut to 200 bytes) and `Retry-After` notes (cut to 64 bytes) | Same-length marker | Kept |

A successful body keeps its length so `content-length` stays correct; the scrubber holds back only a suffix that could still become the start of a key. Scrubbing runs before plugins see the response. Generated forms are sorted longest first, so the formatted credential is blanked before the raw key:

| Form | What is matched |
| --- | --- |
| Raw and formatted | The exact key and the whole `Bearer <key>` value |
| JSON escapes | `\uXXXX` escapes of ASCII letters and digits: all escaped, one at each position, or one per distinct character, in either hex case, plus `\/` |
| Percent encoding | Every byte encoded, only unreserved bytes encoded, one unreserved byte at each position, in either hex case, with `+` for a space |
| Base64 | Standard and URL-safe alphabets, padded and unpadded, and complete characters for a key starting 1 or 2 bytes into a larger value, with edge characters trimmed |
| Split | One ASCII whitespace or control byte at each boundary and between every character; space-separated forms come first |

At most 4096 forms per key; the raw form and up to 2048 split forms are reserved first, so a long key with a space at any boundary is still caught. Keys shorter than 8 bytes are refused at load. Other encodings, compressed bodies and base64 embedded more than two bytes into a larger value are not covered: a response still carrying a coding Reiki did not decode is refused with 502 when a key is in play. A failure-ring snippet that still contains key material after ASCII whitespace and control bytes are removed becomes `[body withheld: contains key material]`. A key split across response chunks is caught. Scrubbing is pattern matching against known key values, not a guarantee for every upstream.

## Network and headers

- The client listener listens on `listen`, default `127.0.0.1:4097`, serves plain HTTP and has no TLS. Keep it on loopback or put a TLS-terminating proxy in front before exposing it.
- Upstream `base_url` values must use `https`, or `http` on a loopback host (`127.0.0.1`, `localhost`, `[::1]`, with an optional port). This permits local mock endpoints without remote plain HTTP.
- Request headers are forwarded except hop-by-hop headers (`connection`, `keep-alive`, `proxy-*`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `host`, `content-length` and client credential headers. `accept-encoding` is replaced with `identity`, unless the upstream sets `forward_accept_encoding` and the client sent one. Upstream redirects are not followed.
- Account-identifying response headers (`openai-organization`, `openai-project`, `anthropic-organization-id`, `set-cookie`, `set-cookie2`) are not relayed.

## Limits and listener enforcement

### Client listener

- Enforces stream-token authentication, request parsing limits and connection caps from `[server]`; loopback is exempt from per-address caps. Failed token checks are counted per peer address and can trigger `[auth]` backoff and 429.
- A stream's `[streams.NAME.limits]` caps admitted requests, tokens, concurrent requests and calendar usage before a key is chosen. `allowed_upstreams` and `allowed_models` globs restrict routes and declared models.
- An intercepted tunnel `CONNECT` must present the stream token in `Proxy-Authorization`; guesses are counted on the same per-address limit, with 429 and `Retry-After` after `max_failures`. A peer address that cannot be read is refused.
- A connection that does not finish its headers within 30 seconds is closed. A proxied request body must arrive within the 120-second request timeout (408).

### Admin listener

- Serves only admin and plugin routes on its own socket. Requests need the full admin token, or on read routes the optional read-only token (`[admin] read_token_sha256`); both are compared by hash in constant time. A read-only token on a mutating route gets 403 and read-only callers get no failure snippets, reload or state error text, or audit reasons. Failed logins are counted per peer address and can trigger `[auth]` backoff and 429.
- Reload and key-enable requests are audited with the credential class and an optional `X-Reiki-Actor` label that never affects authorization. Responses expose operational data such as key names and stream routing, never key values, token hashes or key file paths.
- There is no TLS option: config validation refuses any `[admin].listen` that is not a literal loopback IP and port, including hostnames such as `localhost:9090`. Terminate TLS in front when it must be reachable off-host. The admin token must differ from every stream token, the read-only token from every other credential; the stream name `passthrough` is reserved. See [monitoring](monitoring.md).

### Peer listener

- Serves `GET /v1/health` and `GET /v1/health/stream` on its own socket with a read-only peer token that must differ from admin and stream tokens. Failed logins are counted per peer address and can trigger `[auth]` backoff and 429. Connection and subscriber limits apply and only the configured health summary is published. See [provider health](provider-health.md).

### Streams and keys

| Property | Rule |
| --- | --- |
| Chain state | Streams that list the same key share its cooldown and out-of-rotation state. Run separate instances for mutually untrusted clients. |
| Health influence | A stream-token holder can influence key health: a 401, or a 403 or 400 the upstream attributes to the key, takes that key out of rotation for every stream that lists it. |
| Disables per request | One request disables at most one key. With `hold_unauthorized = true` the disable waits until a later key serves the same request. |
| Invalid-key marker | In a 400 body it counts only as a `reason` string property in parsed JSON, not as echoed text. |

### Dynamic stream administration

`[stream_admin]` is off unless configured. The admin listener then serves `/v1/streams*` with a dedicated bearer token: validation keeps its hash distinct from the admin token, every stream token and every two-party confirmation hash, and requires `allowed_listen` to contain the literal loopback `[admin].listen`. The ordinary admin token does not authorise these routes, and the management token does not authorise other admin routes.

- The raw management token comes from `--token-file` or `token_env` (default `REIKI_STREAM_ADMIN_TOKEN`), never from a CLI argument or the config. `actor_id_sha256` is an audit label, not per-user identity. With `require_two_party`, mutations also need a second raw token from `confirm_env` (default `REIKI_STREAM_ADMIN_CONFIRM_TOKEN`), supplied as `X-Confirm-Token`.
- The API has its own failed-attempt lockout and per-source request budget (`lockout_threshold`, `lockout_secs`, `rpm`). Mutations need an `Idempotency-Key` and a `reason`, reject unknown JSON fields, and stay within the configured ceilings and assignable-key set.
- `streams create` and `streams token add` print a new token once; config, state and audit hold hashes only. Reasons are sanitized before they reach the audit log. There is no HTTP audit route: `reiki streams audit verify` and `reiki streams audit head` read it offline.
- Each audit record carries a sequence, UTC time, actor hash, source, action, stream, outcome, reason, field-level changes and a hash over the canonical JSON of the other fields chained through `prev`. A sibling `audit.head` sidecar holds the last sequence number and hash so verification also detects tail deletion or truncation; without it, verification refuses a non-empty log. A failed audit write refuses the mutation. Verification detects edited, inserted, missing or reordered records.
- The chain does not prove who held the token (the actor hash is a fixed label; `source` is only the peer address on a plain-HTTP listener), that the reason is truthful, or that a token was not copied after its one-time display. It has no external signing key, timestamp authority or append-only storage: anyone who can write the files can replace the log and sidecar and start a fresh chain. Preserve them outside the reach of processes that can call the API.
- State is written atomically with mode `0600` and holds token hashes. An invalid file drops dynamic streams while static streams keep working; startup, reload and restart restore from it. See [dynamic streams](dynamic-streams.md).

## Interception

`[tunnel.intercept]` lets Reiki read the traffic of the hosts it lists; configuration is in [tunnel](tunnel.md#interception). Treat the CA as a secret and clients that trust it as covered by it.

- A CA created by `reiki ca-init` is name-constrained to the listed hosts. A client that trusts it still rejects certificates for other names.
- Keep the CA key readable only by the Reiki service user, use one CA per machine, and install only `ca.pem` in clients.
- Certificates are minted for one exact host and live `leaf_ttl_hours` (default 24). Wildcards and addresses are refused.
- A request is relayed only if its `Host` header matches the tunnel destination and its path lies under the upstream's base path.
- Reiki verifies the upstream certificate on its own TLS connection. Other tunnel destinations stay opaque.
- Plugins see decrypted requests and responses on intercepted hosts. Keep `requestlog`'s `capture_bodies = false` there unless you intend to record bodies; enabling it warns at startup and applies to every sink.

## Logging

Key values and stream tokens are not logged. The service log records no request or response bodies, except the first 160 characters of upstream error bodies at `debug` level. The `requestlog` plugin records method, route, status, durations, sizes, counts, key names and findings, with `capture_bodies = false` and `body = "summary"` by default; opting in writes masked, capped bodies to every configured sink ([requestlog](requestlog.md#what-is-kept-out-of-the-log)). The stream-management audit log is separate: an append-only JSONL file with mode `0600`, sanitized reasons and hashes instead of token values, with no HTTP read route ([dynamic stream administration](#dynamic-stream-administration)).

## Plugins

Plugins are compiled into the binary and run inside the Reiki process, so a plugin is as trusted as Reiki. A plugin sees request headers (including the client token) and bodies but never an upstream key, which is attached after hooks run. Only compile in plugins you have read. Hooks, ordering and panic handling are in [plugins](plugins.md#what-a-plugin-can-see-and-do) and [plugin authoring](plugin-authoring.md). Runtime Wasm plugins are sandboxed by component isolation, fuel, memory and deadline budgets; capabilities, pinning and limits are in [plugin authoring](plugin-authoring.md#runtime-wasm-plugins) and must be treated as security-sensitive.

## Hardening

- Response bodies are capped after decoding (gzip, deflate, brotli, zstd included): `max_decoded_bytes` (default 64 MiB) per upstream. A decoded non-`text/event-stream` body is buffered before its head is sent, so an over-limit body becomes 502 instead of a truncated 200; SSE streams and ends with an error at the cap.
- `response_idle_timeout_secs` (default 300) abandons a body that produces no chunk; optional `response_timeout_secs` bounds the whole body after its head. A buffered timeout answers 504; a streamed body ends with an error.
- A transport failure is replayed only when a side effect cannot be duplicated: any method when the connect itself failed, and GET, HEAD, PUT, DELETE, OPTIONS or TRACE otherwise. A POST or PATCH that may have reached the upstream gets 502 (504 on timeout) instead of a second attempt. An explicit upstream error still fails over for every method. See [failover](failover.md).
- If an upstream error body cannot be read to the end, its key is classified from the status and `Retry-After` alone and the read error is logged without the body.
- Paths with dot segments or encoded separators are refused after up to eight layers of percent decoding; `%252e%252e` and malformed escapes are rejected.
- Client and admin listeners cap open connections and connections per remote address (`[server]`: 1024 and 256, 64 and 16 by default), and close connections that do not finish headers within 30 seconds. Dropped-connection warnings are rate-limited.
- The `placeholders` plugin keeps credential placeholders masked inside tool-call arguments unless `restore_tool_args` is set.
- The key-health state file is written mode `0600` through an exclusively created, uniquely named temporary file and holds a SHA-256 of each key, never a value. At startup Reiki probes the state directory and, when `[log] file` is configured, the log directory with `create_new` and `O_NOFOLLOW`; a planted file or symlink cannot satisfy the check. With `[gateway] on_store_error = "closed"` (default) a failed probe exits before binding; `"open"` logs a warning and continues.
- A config syntax error is reported without source text, so a secret in a malformed line does not reach `/v1/status`, `/v1/reload` or the log.

## Supply-chain and token scope

A compromised dependency in an agent framework runs with that framework's privileges. If the framework can read its config or environment, it can read the stream token and present it to Reiki. Treat the token as a bearer secret for one stream, not as proof of a trusted process.

- A stolen stream token selects that stream's key chain, can spend those keys, influence key health for every stream listing them, and authorises intercepted `CONNECT` hosts mapped to that stream. It does not open the admin or peer listeners or reveal the upstream key value. A framework configured to call a provider directly never reaches Reiki; see [clients](clients.md).
- Tokens are stored as hashes and compared in constant time, but a valid token from any source address authenticates. The peer address is used only for failed-login backoff and connection caps. Enforce source restrictions at the TLS terminator in front of the client listener.
- `[streams.NAME]` has no expiry field or runtime TTL. Rotate by changing `token_sha256` and reloading: new authentications with the old token fail, gateway counters start fresh and peer SSE subscriptions opened with a rotated token close. A request or tunnel already authorised keeps its state until it closes. Keep token lifetime and rotation cadence in provisioning, and use separate instances for mutually untrusted clients.
- The `[tunnel]` `allow` list gates destinations by host and port only. A plain allow-listed `CONNECT` needs no stream token; removing the token does not close it. Intercepted hosts check the mapped stream's token, but their allow-list entry is global. Keep the listener on a restricted network and `deny_private = true`. See [tunnel](tunnel.md#trust-boundary).
- Cloud-token detection in bodies is not a barrier. The `phi`, `finance` and `controlled` plugins only detect their configured kinds; `placeholders` masks credential-shaped strings in requests, and `requestlog` masks bodies only with `capture_bodies = true`. Response scrubbing removes only Reiki's known upstream key. See [plugins](plugins.md#placeholders) and [plugins](plugins.md#detection-plugins).

## What Reiki does not protect against

Reiki controls traffic that reaches its listeners. It is not a sandbox for the client, a secret store for the host, or proof that every model call used the proxy.

| Threat | Covered by | Not covered |
| --- | --- | --- |
| An agent or another host reading a key file | Private directory, file mode `0600`, `umask 077`, default `strict_key_permissions = true` | Processes running as the Reiki user or root; `strict_key_permissions = false` turns refusal into a warning |
| A client getting an upstream key | Client credential headers and `key` query removed; upstream credential set per attempt; [redaction](#clients-and-keys) | `passthrough` upstreams, and echoes outside the scrubber's form list |
| A plugin reading or changing a request | Compiled-in plugins never receive an upstream key | A plugin is trusted code with Reiki's privileges; panics are caught, not sandboxed |
| Traffic to arbitrary destinations | Config-only upstreams and the [tunnel allow-list](tunnel.md#trust-boundary) | Opaque CONNECT destinations; `allow = ["*"]` is legal and leaves them unread |
| An echoed key in a response | Finite form list, including whitespace splits and common encodings | Other encodings, compressed bodies and base64 embedded beyond two bytes |
| An identifier in a request body | `phi`, `finance`, `controlled` and `placeholders` ([plugins](plugins.md)) | Headers, query strings and responses; base64 `image`/`document`, `thinking` and `signature` blocks |
| A prompt or logged identifier reaching a log reader | `requestlog` summaries by default; captured bodies masked and capped | Pattern masking misses secrets in prose and personal data; a detector in `report` mode with body capture writes identifiers |
| A plugin failure during masking | `placeholders` fails closed by default | `on_error = "pass_through"` lets the unmasked request continue |
| Traffic that never reaches Reiki | Stream token, one route per upstream prefix, counters | Clients with their own key and base URL, sub-agents and utility models; see [clients](clients.md) |
| One client affecting shared keys | `hold_unauthorized` and separate instances | A stream-token holder can still disable a key for every stream that lists it |

Other goals need controls outside Reiki: prove no client call went direct (enforce egress and process boundaries), keep keys from every host process (separate account or secret store), scan every outbound byte (restrict to intercepted hosts), and trust a third-party plugin (review and build it yourself).

## Security scanning

Workflows in `.github/workflows` run the checks below; results appear as pull-request checks, code scanning alerts or release assets.

| Workflow | Runs on | Checks |
| --- | --- | --- |
| `ci` | Pushes, pull requests, manual | Format, clippy, repository hygiene, tests with and without `top` |
| `codeql` | Pushes to `main`, pull requests, weekly | CodeQL for Rust, build mode `none` |
| `scorecard` | Pushes to `main`, weekly | OpenSSF Scorecard, published to the Scorecard API |
| `dependency-review` | Pull requests | New high or critical advisories and licences outside `deny.toml` |
| `audit` | Pushes, pull requests, daily | `cargo deny check`: advisories, bans, licences and sources |
| `fuzz` | Weekly, manual | Ten cargo-fuzz targets over untrusted-input parsers |
| `release` | `v*` tags | Archives, CycloneDX SBOM with checksums, archive attestations |

The dependency-review licence list mirrors `deny.toml`. Maintainers can check workflow YAML, pinned action SHAs and job permissions with `cargo run --quiet --manifest-path scripts/check-workflows/Cargo.toml`.

### Repository hygiene

`scripts/check-repo-hygiene.py` (also `make hygiene`) scans tracked files for private development hostnames and internal domain suffixes. A finding fails CI. Add an entry to `scripts/check-repo-hygiene.allowlist` only for a match that is not a hostname, with a comment explaining why. Examples use documentation IP ranges and neutral upstream names.

### Fuzzing

`fuzz/` is a separate Cargo workspace. Targets cover SSE lines and chunk boundaries, scrub form generation, plugin masking, the JSON key-invalid marker, config and key-file parsing, repeated percent decoding and dot segments, tunnel allow-lists, and bearer-token and rate-limit headers. Each asserts that the parser does not panic, secrets do not survive scrubbing, and output stays bounded by input. Run one target with:

```bash
cd fuzz
cargo +nightly fuzz run sse_parser -- -max_total_time=30
```

Install the pinned driver with `cargo install cargo-fuzz --locked --version 0.13.2`. The workflow runs every target for 120 seconds and uploads reproducing bytes on failure. The `fuzzing` feature only exposes internals to the harnesses; it adds no `unsafe` code.

### Verify a release

```bash
tag=<version>
gh release download "$tag" --repo ddproxy/reiki \
  --pattern '*.tar.gz' --pattern '*.sha256' --pattern '*-sbom.json' --dir dist
gh attestation verify "dist/reiki-$tag-x86_64-unknown-linux-musl.tar.gz" --repo ddproxy/reiki
(cd dist && shasum -a 256 -c *.sha256)
```

`gh attestation verify` checks an archive against the attested provenance; the `.sha256` files cover archives and the SBOM. Run one verification per archive you use.

### Owner checklist

These are GitHub settings, not repository files:

- Enable secret scanning, push protection and private vulnerability reporting.
- Enable Dependabot alerts and the dependency graph.
- Protect `main`: require a pull request and the checks `test`, `Analyze (rust)`, `dependency-review` and `Audit`, with branches up to date. Do not require `Scorecard`; it runs on pushes and weekly.
- Keep code scanning default setup disabled; CodeQL and Scorecard upload SARIF from the workflows above.
- Register for an OpenSSF Best Practices badge. Do not add a crates.io badge: `Cargo.toml` sets `publish = false`.
