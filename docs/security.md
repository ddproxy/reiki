# Security

Reiki sits between a client (an agent such as opencode or Claude Code, or an application) and the providers it calls. It keeps upstream credentials with Reiki and applies configured controls to traffic that passes through it, so each side receives only what that route and policy allow. Masking and filtering complement controls that keep secrets out of reach on the client. Trust boundaries and limits are described below.

| Reiki keeps | From | Through |
| --- | --- | --- |
| Upstream API keys | The client | Key files read by Reiki; credential headers replaced per request |
| Credentials and listed terms in prompts | The provider | The `placeholders` plugin ([placeholders](placeholders.md)) swaps them for placeholders before the request leaves and restores them in the response |
| Arbitrary destinations | The client | Config-only upstreams and the tunnel allowlist |
| A record of what was sent | Nobody; it is for you | The `requestlog` plugin ([requestlog](requestlog.md)), which records summaries by default and masks any bodies you explicitly opt in to capture |
| Dynamic stream management | A holder of the dedicated stream-admin token | A separate credential on the admin listener, strict JSON requests and a hash-chained JSONL audit log |

## Clients and keys

- Clients never see upstream keys, unless an upstream echoes the key back in a form Reiki does not recognise. In error bodies, headers relayed to the client, and successful responses (headers and body, streamed or not) Reiki replaces the exact key, its formatted credential, and their JSON-escaped, percent-encoded and base64 (standard and URL-safe, padded or not) forms with `[redacted]`. That covers `\uXXXX` escapes of ASCII letters and digits (all of them, one at each position, or one per distinct character), every byte percent-encoded, only unreserved characters percent-encoded, a single character at each position, and base64 at all three byte alignments, with the edge characters of an embedded key that depend on surrounding bytes trimmed. A key split by any ASCII whitespace or control byte, at one boundary or between every character, is caught, as is one split across two chunks. A successful body keeps the replacement's length, so a content length stays correct. Other encodings, compressed bodies, and base64 embedded more than two bytes into a larger value are not covered; a response that still carries a coding Reiki did not decode is refused with 502 when a key is in play, rather than relayed with an echoed key unscrubbed. The form list is capped at 4096 per key and built lazily within per-category budgets; the raw form and up to 2048 split forms are reserved for every key, space-separated forms come first, and base64 forms are built before other encodings, so a long key with a space at any boundary is still caught without generating unbounded variants. Plugins see the response after this step. A configured key over 4096 bytes is refused at load; a key shorter than 8 bytes is refused at load too: an echoed key that short would never be scrubbed. Reiki removes the client's credential headers (`authorization`, `x-api-key`, `x-goog-api-key`) and any `key` query parameter, then sets the upstream's header from the key file; a passthrough upstream receives the client's own credentials and query string unchanged.
- The key travels to whoever accepts on an `[upstream.X.forward]` socket or runs as its `command`, only in the normal credential header, with no second copy. A command gets an environment cleared except `PATH`, `HOME`, `LANG`, `LC_ALL` and the names in `pass_env`. Keep the socket in a directory only the Reiki user and listener can enter, and set `expect_uid`, which makes Reiki compare the uid of the process that accepted the connection (peer credentials, not the socket file's owner) and refuse a mismatch. It is optional, so without it whoever binds the path receives the keys; to bound that, a socket whose parent directory is group- or other-writable and lacks the sticky bit is refused at load when `expect_uid` is unset (sticky directories such as /tmp stay allowed). A failed link to the forwarder answers 502 and never cools the key.
- Clients never see upstream keys. An upstream can echo one; Reiki scrubs the known forms from relayed headers, error bodies, successful bodies and failure-ring snippets. The [redaction reference](redaction.md) lists the exact forms, budgets and limits.
- Client credential headers (`authorization`, `x-api-key`, `x-goog-api-key`) and the `key` query parameter are removed on key-backed routes. Reiki sets the upstream's header from the key file. A `passthrough` upstream receives the client's own credentials and query string unchanged.
- A key travels to whoever accepts on an `[upstream.X.forward]` socket or runs as its `command`, only in the normal credential header, with no second copy. A command gets an environment cleared except `PATH`, `HOME`, `LANG`, `LC_ALL` and the names in `pass_env`.
- Keep the socket in a directory only the Reiki user and listener can enter, and set `expect_uid`. Reiki compares the uid of the process that accepted the connection (peer credentials, not the socket file's owner) and refuses a mismatch.
- `expect_uid` is optional: without it, whoever binds the path receives the keys. A socket whose parent directory is group- or other-writable and lacks the sticky bit is refused at load when `expect_uid` is unset; sticky directories such as `/tmp` stay allowed. A failed link to the forwarder answers `502` and never cools the key.
- Stream tokens are stored as SHA-256 hashes and compared in constant time across all streams.
- `reiki hash-token` refuses a token shorter than 16 characters. A `token_sha256` is verified only as 64 hex characters; a hash does not reveal the token's length, so a token set another way is not checked. Use at least 16 random characters.
- Upstreams come only from the config, so a request cannot choose its own destination and Reiki is not an open proxy.
- Upstream key files are read at startup and again on SIGHUP or reload. Their mode is checked on the same handle used to read them: with the default `strict_key_permissions = true`, a file with group or other read or write bits is a load error (`reiki keys --check` reports it too); set it to `false` to keep the old warning-only behaviour. Create key files mode 0600.
- Upstream key files are read at startup and again on SIGHUP or reload. The daemon trims the value and requires 8 to 4096 bytes. With the default `strict_key_permissions = true`, startup and reload refuse a key file that group or other users can read or write; set it to `false` to warn instead. `reiki keys --check` always reports such a file as a warning and exits `1`. Create key files mode 0600.
- The interception CA private key (`[tunnel.intercept].ca_key`) is checked for group or other access when it is loaded, according to `ca_key_permissions` (`error` or `warn`), during startup, `reiki check` and a reload that reads the CA (`src/intercept.rs`). The peer listener TLS private key (`[peers].tls_key`) is checked the same way according to `[peers].tls_key_permissions` during startup, `reiki check` and a reload that reads the peer TLS files (`src/peers/tls.rs`). Keep these keys private to the Reiki user.

## Dynamic stream administration

`[stream_admin]` is off unless configured. With it, the admin listener serves `/v1/streams*` with a dedicated bearer token: config validation keeps its hash distinct from the admin token, every stream token and every two-party confirmation hash, and requires `allowed_listen` to contain the literal loopback `[admin].listen`. The ordinary admin token does not authorise these routes, and the management token does not authorise other admin routes.

- **Credential handling.** The raw management token comes from `--token-file` or `token_env` (default `REIKI_STREAM_ADMIN_TOKEN`), never from a CLI argument or the config. `actor_id_sha256` is a separate configured hash used only as an audit label, not per-user identity. With `require_two_party`, mutations also need a second raw token from `confirm_env` (default `REIKI_STREAM_ADMIN_CONFIRM_TOKEN`), supplied as `X-Confirm-Token`.
- **Bounded, strict mutations.** The management API has its own failed-attempt lockout and per-source request budget (`lockout_threshold`, `lockout_secs`, `rpm`). Mutations need an `Idempotency-Key` and a `reason`, reject unknown JSON fields, and stay within the configured ceilings and assignable-key set. Denied attempts are rate-limited.
- **Secrets and redaction.** `streams create` and `streams token add` print a new token once; config, state and audit hold hashes only. Reasons are sanitized before they are written to the audit log. There is no HTTP audit route: `reiki streams audit verify` and `reiki streams audit head` read the log offline.
- **Audit guarantee.** Each record carries a sequence, UTC time, actor hash, source, action, stream, outcome, reason, field-level changes and a hash over the canonical JSON of the other fields chained through `prev`. When present, a sibling `audit.head` sidecar holds the last sequence number and hash so verification also detects tail deletion or truncation; without it, verification refuses a non-empty log. A failed audit write refuses the mutation. Verification detects edited, inserted, missing or reordered records.
- **What the chain does not prove.** It does not prove who held the token (the actor hash is a fixed label; `source` is only the peer address on a plain-HTTP listener), that the reason is truthful, that a token was not copied after its one-time display, or anything about proxied traffic. It has no external signing key, timestamp authority or append-only storage: anyone who can write the files can delete or replace the log and its sidecar, and a replacement starts a fresh chain. Preserve logs and sidecars outside the reach of processes that can call the API.
- **State.** The state file is written atomically with mode `0600` and holds token hashes. An invalid file drops dynamic streams while static streams keep working; startup, reload and restart restore from it. See [upgrades](upgrades.md#dynamic-stream-state).

## Network

Reiki listens on `listen`, default `127.0.0.1:4097`, serves plain HTTP and has no TLS. Keep it on loopback, or put a TLS-terminating proxy in front before exposing it. Upstream `base_url` values must use `https`, or `http` on a loopback host (`127.0.0.1`, `localhost` or `[::1]`, with an optional port). This allows local mock and development endpoints without allowing plain HTTP to remote hosts.

## Header handling

Reiki forwards every request header except hop-by-hop headers (`connection`, `keep-alive`, `proxy-*`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `host`, `content-length` and the client credential headers. `accept-encoding` is replaced with `identity` so error bodies can be classified, unless the upstream sets `forward_accept_encoding` and the client sent one. Redirects from the upstream are not followed.

## Interception

`[tunnel.intercept]` lets Reiki read the traffic of the hosts it lists, so treat the CA as a secret and the clients that trust it as covered by it.

- The CA created by `reiki ca-init` is name-constrained to the hosts you give it. A client that trusts it still rejects a certificate for any other name, so a stolen CA key cannot impersonate other sites.
- The CA key must be readable only by the Reiki service user. Use one CA per machine, never share one across hosts, and install only `ca.pem` in clients.
- Certificates are minted for one exact host and live `leaf_ttl_hours` (default 24). Wildcards and addresses are refused.
- A request is relayed only if its `Host` header matches the tunnel destination and its path lies under the upstream's base path; anything else is refused. This keeps one tunnel from reaching other hosts or other parts of the API.
- Reiki verifies the upstream's certificate on its own TLS connection, as it does for every other route.
- Plugins see decrypted requests and responses on intercepted hosts. Keep `requestlog`'s `capture_bodies = false` there unless you intend to record the bodies; enabling body capture warns at startup and applies to every sink.
- Other tunnel destinations stay opaque and are not read.

## Logging

Key values and stream tokens are not logged. Reiki's service log does not record request or response bodies, except the first 160 characters of upstream error bodies at `debug` level. The `requestlog` plugin records method, route, status, durations, sizes, counts, key names and failure findings, but no body text by default: `capture_bodies` defaults to `false`, and setting it to `true` warns at startup. Opting in lets `body = "full"` and `response = "body"` write request, response and failed-attempt bodies to every configured sink; those bodies are masked in paths, query parameters, credential headers and JSON keys and values, and capped per entry. A failure-ring snippet is scrubbed the same way as a relayed body and cut to 200 bytes; if a compacted copy still contains key material after ASCII whitespace and control bytes are removed, the whole snippet becomes `[body withheld: contains key material]`. The optional stream-management audit log is separate from all of this: an append-only JSONL file created with mode `0600`, whose reasons are sanitized and whose records hold hashes rather than token values; it has no HTTP read route. See [dynamic stream administration](#dynamic-stream-administration).

## Plugins

Plugins are compiled into the binary and run inside the Reiki process, so a plugin is as trusted as Reiki. A plugin sees request headers (including the client token) and bodies but never an upstream key, which is attached after the hooks run. Only compile in plugins you have read. [Plugins](plugins.md#what-a-plugin-can-see-and-do) covers the hooks and panic handling.

## Limits and listener enforcement

- **Client listener.** Enforces stream token authentication, request parsing limits and connection caps from `[server]` (with loopback exempt from per-address caps). Failed token checks are counted per peer address and can trigger `[auth]` backoff and `429` responses on that listener. A stream's `[streams.NAME.limits]` caps admitted requests, tokens, concurrent requests and calendar usage for that stream before any key is chosen, and its `allowed_upstreams` / `allowed_models` globs restrict which routes and declared models it may use. An intercepted tunnel `CONNECT` must present the stream token in `Proxy-Authorization`; guesses are counted on the same per-address limit as client requests, with `429` and `Retry-After` after `max_failures`. Refused destinations follow tunnel rules. A peer address that cannot be read is refused rather than checked without throttling.
- **Admin listener.** Serves only the admin and plugin routes on its own socket. Requests need the full admin token, or on read routes the optional read-only token (`[admin] read_token_sha256`); both are compared by hash in constant time. A read-only token on a mutating route gets `403`, and read-only callers get no failure snippets, no reload or state error text and no audit reasons. Reload and key-enable requests are audited with the credential class and an optional `X-Reiki-Actor` label that never affects authorization. Failed admin logins are counted per peer address and can trigger `[auth]` backoff and `429` responses on that listener (`src/admin.rs`). It has no TLS option, so config validation refuses any `[admin].listen` that is not a literal loopback IP and port (hostnames such as `localhost:9090` are refused too): bind it on loopback and terminate TLS in front when it must be reachable off-host. Responses expose operational data such as key names, upstream names and stream routing, but not key values, token hashes or key file paths.
- **Peer listener.** Serves `GET /v1/health` and `GET /v1/health/stream` on its own socket with a read-only peer token that must differ from admin and stream tokens. Failed peer logins are counted per peer address and can trigger `[auth]` backoff and `429` responses on that listener (`src/peers/publisher.rs`). The listener enforces peer connection and subscriber limits and publishes only the configured health summary. See [peers](peers.md#what-a-peer-token-holder-learns).
- **Streams and keys.** A stream's chain is shared state: streams that list the same key share its cooldown and out-of-rotation state. Run separate instances for mutually untrusted clients. A stream-token holder can influence key health: a 401, or a 403 or 400 the upstream attributes to the key, takes that key out of rotation for every stream that lists it until it is rotated and reloaded. One request disables at most one key; with `hold_unauthorized = true`, the disable waits until a later key serves the same request. The invalid-key marker in a 400 body counts only as a `reason` string property in parsed JSON, not as echoed text.
- **Admin listener.** Serves only the admin and plugin routes on its own socket. Requests need the full admin token, or on read routes the optional read-only token (`[admin] read_token_sha256`); both are compared by hash in constant time. A read-only token on a mutating route gets `403`, and read-only callers get no failure snippets, no reload or state error text and no audit reasons. Reload and key-enable requests are audited with the credential class and an optional `X-Reiki-Actor` label that never affects authorization. Failed admin logins are counted per peer address and can trigger `[auth]` backoff and `429` responses on that listener (`src/admin.rs`). It has no TLS option, so config validation refuses a non-loopback `[admin].listen`: bind it on loopback and terminate TLS in front when it must be reachable off-host. Responses expose operational data such as key names, upstream names and stream routing, but not key values, token hashes or key file paths.
- **Peer listener.** Serves `GET /v1/health` and `GET /v1/health/stream` on its own socket with a read-only peer token that must differ from admin and stream tokens. Failed peer logins are counted per peer address and can trigger `[auth]` backoff and `429` responses on that listener (`src/peers/publisher.rs`). The listener enforces peer connection and subscriber limits and publishes only the configured health summary. See the [health document schema](peers.md#health-document-schema).
### Shared state

| Property | Rule |
| --- | --- |
| Chain state | Streams that list the same key share its cooldown and out-of-rotation state. Run separate instances for mutually untrusted clients. |
| Health influence | A stream-token holder can influence key health: a 401, or a 403 or 400 the upstream attributes to the key, takes that key out of rotation for every stream that lists it until it is enabled again or its file changes and is reloaded. |
| Disables per request | One request disables at most one key. With `hold_unauthorized = true`, the disable waits until a later key serves the same request. |
| Invalid-key marker | In a 400 body it counts only as a `reason` string property in parsed JSON, not as echoed text. |

## Hardening

- Account-identifying response headers (`openai-organization`, `openai-project`, `anthropic-organization-id`, `set-cookie`, `set-cookie2`) are not relayed.
- A response body is capped after decoding (gzip, deflate, brotli or zstd included), so a small compressed body cannot expand without bound. The cap is per upstream with `max_decoded_bytes` (default 64 MiB). A decoded response that is not `text/event-stream` is buffered before its head is sent, so an over-limit body becomes a 502 instead of a truncated 200. SSE still streams and ends with an error if it passes the cap.
- Response bodies are bounded in time too: `response_idle_timeout_secs` (default 300) abandons a body that produces no chunk, and optional `response_timeout_secs` bounds the whole body after its head. A buffered decoded body that runs out of time answers 504; a streamed body ends with an error, so a stalled upstream cannot hold a client connection forever.
- The `placeholders` plugin keeps credential placeholders masked inside tool-call arguments unless `restore_tool_args` is set.
- A transport failure is replayed only when replay cannot duplicate a side effect: any method when the connect itself failed, and GET, HEAD, PUT, DELETE, OPTIONS or TRACE otherwise. A POST or PATCH whose request may have reached the upstream is answered with 502 (504 on timeout) instead of being sent again on another key. An explicit upstream error response still fails over for every method.
- If an upstream error body cannot be read to the end, its key is classified from the status and `Retry-After` alone, and the read error is logged without the body.
- Request paths with dot segments or encoded separators are refused, after repeated percent-decoding (up to eight layers). Double-encoded forms such as `%252e%252e` are rejected, as are malformed percent escapes.
- The client and admin listeners close a connection that does not finish its headers within 30 seconds, cap open connections and open connections per remote address (`[server]`: 1024 and 256 on the client listener, 64 and 16 on the admin one, by default; loopback is exempt from the per-address caps because every local process shares `127.0.0.1`; the dropped-connection warning is rate-limited). A proxied request body must arrive within the 120-second request timeout (408).
- The admin token must differ from every stream token, the read-only admin token must differ from every other credential, and the stream name `passthrough` is reserved.
- The key-health state file is written with mode 0600 through an exclusively created, uniquely named temporary file. It holds a SHA-256 of each key to detect rotation, never a key value. At startup Reiki probes the state directory and, when `[log] file` is configured, the log directory by creating, writing a few KB to, and removing an unpredictable file with `create_new` and `O_NOFOLLOW`; a planted file or a symlink at the probe path cannot satisfy the check or redirect the write. With `[gateway] on_store_error = "closed"` (default) a failed probe exits before binding; with `"open"` Reiki logs a `WARN` and continues.
- A config syntax error is reported without the source text, so a secret in a malformed line does not reach `/v1/status`, `/v1/reload` or the log.

### Agent-framework supply-chain scenario

A compromised dependency in an agent framework runs with that framework's privileges. If the framework can read its config or environment, it can read the stream token and present it to Reiki. Treat the token as a bearer secret for one stream, not as proof of a trusted process.

- **What a stolen stream token buys.** It selects that stream's key chain on the client listener and can spend those keys. Because chains are shared state, it can also influence key health for every stream that lists the same keys, and it authorises intercepted `CONNECT` hosts mapped to that stream. It does not open the admin or peer listeners: those require different token hashes. It does not hand over the upstream key value; echoes are scrubbed as described in [redaction](redaction.md). A framework configured to call a provider directly, with its own key and base URL, never reaches Reiki at all; see [limits](limits.md).
- **Source binding is not provided.** Tokens are stored as hashes and compared in constant time, but a valid token from any source address authenticates. The peer address is only an input to failed-login backoff and connection caps, never a token constraint. The client listener serves plain HTTP and has no client-certificate identity of its own, so an upstream TLS terminator is the place to enforce source restrictions. A stolen token therefore works from any host that can reach that listener.
- **Tokens are not short-lived.** `[streams.NAME]` has no expiry field and no runtime TTL check. Rotate a stream by changing `token_sha256` and reloading: new authentications with the old token then fail, the stream's gateway counters start fresh, and peer SSE subscriptions opened with a rotated peer token are closed. A request or intercepted tunnel already authorised with the old token is not retroactively aborted; it keeps its state until it finishes or closes. Keep token lifetime and rotation cadence in the operator's provisioning process, and use separate instances for mutually untrusted clients as described under [shared state](#shared-state).
- **Tunnel allow-lists and exfiltration.** The `[tunnel]` `allow` list gates destinations by host and port only. A plain allow-listed `CONNECT` needs no stream token at all, so stealing one does not widen that path; conversely, removing the token does not close it. Opaque destinations are relayed byte for byte and are not inspected. Intercepted hosts are checked against the mapped stream's token, but their allow-list entry is global. With `deny_private = true`, private and link-local addresses are refused even when `allow` is broad. Keep the tunnel listener on a restricted network, keep `allow` as narrow as the clients need, and do not treat it as a per-stream permission system. See [tunnel](tunnel.md#trust-boundary).
- **Cloud-token detection in bodies is not a barrier.** The PHI, finance and controlled-information plugins detect their own kinds in request bodies when compiled and configured; there is no separate core detector that recognises infrastructure or cloud credentials in arbitrary bodies. `placeholders` masks credential-shaped strings in requests, and `requestlog` masks them in bodies only when `capture_bodies = true`; both are bounded heuristics, not a guarantee that an unknown token format is caught. Scrubbing of response bodies and failure-ring snippets only removes Reiki's *known upstream key*, not other secrets an upstream or framework may echo. Do not assume a leaked infrastructure token in a body is blocked or reported.

Adding source binding, token expiry and a general credential-detection kind would change the config surface and detection model; those are owner decisions, not current behaviour.

## Wasm plugins (Phase 1)

Runtime Wasm plugins (`plugin-wasm`) run inside the Reiki process but are
sandboxed by Wasmtime component isolation, fuel, memory limits, and
epoch/deadline budgets. Phase 1 exposes only request-side hooks and a
constrained host interface.

- **Pinning.** Config requires `sha256`; load fails on mismatch. The embedded
  `reiki-manifest` custom section must match the configured name and the
  component import-derived capability request.
- **Capability derivation.** Capabilities are derived from imports (D1). Gated
  capabilities (`request-body:read`, `request-body:edit`,
  `finding:detail-vocab`) require explicit config grants; requesting more than
  granted fails to load. Default-allowed capabilities are low-risk and linked
  for every plugin.
- **No filesystem or network capability.** Components importing
  `wasi:filesystem`, `wasi:sockets`, or `wasi:http` are refused at load, and no
  grant can enable them. The runtime pins `wasmtime`/`wasmtime-wasi` 51.0.0.
  End-to-end tests check that a reference plugin cannot open, truncate, rename,
  or hard-link files through WASI, and that a component importing filesystem
  or socket interfaces is refused.
- **Resource limits/traps.** Fuel, memory, and deadlines are enforced per call.
  Traps fail closed for `role = "security"` or `on_error = "reject"`, and are
  logged otherwise.
- **Host-applied edits.** Plugins return structured edit ops with manifest
  literals/JSON pointers; the host applies and revalidates JSON. Free-form
  rewrite is not supported in Phase 1.
- **Request-side tool allowlist.** Host config can enforce a declared-tool
  allowlist on recognized request formats, reusing `reiki-toolpolicy` request
  parsing. Unparseable recognized requests fail closed.
- **Reload narrowing.** Reload cannot widen grants, limits, tool allowlists, or
  high-risk stream scope; widening requires restart.

Wasm plugins are not a substitute for process isolation. Treat plugin
configuration, manifests, and granted capabilities as security-sensitive.
