# Changelog

## 0.9.0

### Added

- An optional `[peers]` table for health sharing between instances, behind a new `peers` cargo feature that is in the default set. It is off unless `enabled = true`: without the table, or with it disabled, no listener is opened, no peer is contacted, and `/status` and `/v1/openapi.json` are unchanged (golden and socket-table tests check this). A binary built without the feature refuses a `[peers]` table. See [peers](docs/peers.md).
- `[peers]` validation: unknown keys are refused, publishing needs its own `listen` (loopback unless TLS is on) and a `peer_token_sha256` that differs from the admin token and every stream token, and a peer `url` must be https with a `ca_file`, or http on loopback.
- `reiki check` prints a `peers:` line with the effective settings, or `peers: disabled`.
- `GET /v1/health` on a separate peer listener (`[peers] listen`), served only with `enabled` and `publish`. Per upstream it reports `healthy_keys`, `cooling_keys`, `disabled_keys`, `soonest_cooldown_seconds` and `last_status_class`, plus `instance` and `observed_at`; key labels only with `expose_key_labels`. Only the peer bearer token opens it, failures are logged with the peer address and throttled with counts of their own (see the loopback entry below), and every other path is 404. The route is in `/v1/openapi.json` while publishing, with the peer listener as its server and a `peerAuth` scheme.
- `[peers] instance` names the published instance; `reiki` otherwise. Changing the peer listener shows as `peers.listen` in a reload's `restart_needed`.
- `[peers] expose_host_stats` (off by default) adds `host` (load average, cores, memory and swap, disk use per role for the config, state, log and request log directories, never paths) and `sessions` to `/v1/health`. `active_sessions` counts distinct values of `session_header` (default `x-opencode-session`) over `session_window_secs` (default 300), once per client request, kept only as salted hashes in a bounded table. A per-stream breakdown needs `expose_key_labels` too.
- The `peers` feature adds the `rustix` crate (unix only) for `statvfs`.
- `reiki peers` (and `--json`) reads `/v1/health` from each `[peers.<name>]`. Every peer gets its own client that trusts only its `ca_file`, sends only the peer bearer token, follows no redirect, gives up after 5 s and refuses a body over 64 KiB. A failure, or data more than three poll intervals old, shows as `unknown` and never as healthy. It exits 1 when no peer gave fresh data. A token file that group or others can read is refused.
- `GET /v1/health/stream` on the peer listener, with the same token: Server-Sent Events with a `snapshot` on connect, `key` when an upstream's key counts or states change, `host` every `stream_interval_secs` (default 5) and `sessions` on change with `expose_host_stats`, and a heartbeat comment every 15 s. Events go through one bounded broadcast; a subscriber that stops reading is dropped. `max_subscribers` (default 16) and `max_subscribers_per_peer` (default 4) cap open streams. In `/v1/openapi.json` while publishing.
- `reiki peers --watch` follows each peer's stream and reprints on every change, polling `/v1/health` while a stream is down.
- On the peer listener, connections from loopback (tunnels and local TLS proxies, so every peer of a loopback listener) no longer share a per-address lockout or the `max_subscribers_per_peer` cap: their failures go into one shared count that delays each failed attempt by 500 ms and never refuses a valid token, and their streams are capped per token. Connections from other addresses keep the per-address lockout. A successful peer login no longer clears the failure count; the client and admin listeners are unchanged.
- The stream's change checker runs only while a stream is open and publishing is on, and ends when the last subscriber leaves or a reload stops publishing, instead of polling (and reading host stats) for as long as the process lives.
- Host stats query filesystems on the blocking pool with a 2 s timeout instead of on the async runtime; a role that errs or times out is reported with `null` figures, and a hung directory is not queried again until its call returns.
- `/v1/health` and stream `key` events carry `age_seconds`, and `reiki peers` judges freshness by it plus its own time since receipt, so clock skew between hosts no longer turns a peer `unknown`; `observed_at` stays and is only used against this host's clock for a peer that sends no age. The `observed_at` parser now takes fractional seconds, lowercase `t`/`z` and numeric offsets, using the `parsing` feature of the existing `time` dependency (no new crate, `Cargo.lock` unchanged).
- Without `[peers] instance` the published `instance` is the fixed name `reiki` instead of the host name, so host, container and node names are not published.
- The release workflow checks that each natively runnable binary prints `peers: disabled` from `reiki check` with no `[peers]` table and with `enabled = false`.
- `[peers] cooldown_bucket_secs` (default 300, 0 for exact) rounds `soonest_cooldown_seconds` and the labelled `cooldown_seconds` up to a multiple of it, in `/v1/health` and in stream events.
- `[peers] tls_cert` and `tls_key` serve the peer listener over HTTPS with rustls, from a certificate any CA issued. Both or neither; the key must not be accessible to group or others (`tls_key_permissions = "warn"` loads it with a warning, as for the interception CA key). Without them a non-loopback `listen` is refused. `reiki check` validates the files and prints the certificate's days left, and a reload swaps a renewed certificate and key without a restart (`peers.tls` in `changed`); switching TLS on or off is `peers.tls` in `restart_needed`.
- `[[peers.tokens]] (`name`, `token_sha256`) adds named peer tokens beside `peer_token_sha256`, which is the token `default`. Any of them opens the peer listener; all are compared on every request. Names are `[a-z0-9-]`, at most 32 bytes, unique, and hashes must differ from each other, the admin token and every stream token. `max_subscribers_per_token` (default 8) caps streams per token. A reload applies token changes at once and ends the streams of a removed or changed token.

## 0.8.3

### Fixed

- The x86-64 Linux release binaries 0.8.0 to 0.8.2 used AVX instructions in the TLS library's CPU detection and crashed with an illegal instruction on CPUs without them (a Nehalem Xeon and a Sandy Bridge Core i3 were affected). The release now builds the C code for baseline x86-64 and the workflow runs `ca-init` on an emulated Nehalem CPU before publishing.

### Added

- `requestlog` option `max_capture_bytes` (default 1048576, 1 to 67108864) caps the body bytes copied per request onto the writer queue, which bounds the queue's worst case. A request body over it is logged as masked text with `body_truncated`. Before, the whole body was queued; the response body and error body capture now also stop at it.

- `reiki_plugin_rejections_total{plugin,stream}` counts requests a plugin rejected, or skipped under `on_error = "reject"`. The plugin API gains `Event::PluginRejected`.

- `hold_unauthorized = true` on an upstream (default `false`) holds the disable of the first key that answers 401 until a later key serves the same request, so a request every key refuses cannot take a good key out of rotation.

### Fixed

- `requestlog` keeps writing to the current file when a log rotation fails (a rename error), warns once and retries after a growing pause. The failed record used to be lost and every record retried the rotation.
- A non-streamed JSON response that carries tool calls is no longer re-serialised by the `placeholders` plugin. Only the strings that change are rewritten, so key order and formatting are kept.

### Security

- `requestlog` masks more: provider key prefixes (Stripe, Hugging Face, Groq, xAI, Perplexity, NVIDIA, npm, PyPI, Shopify, SendGrid and others), base64 and percent-encoded keys, string values of fields named like credentials, secrets split across stream deltas, and a secret cut at a capture or truncation point. The added prefixes also apply to the `placeholders` plugin, which shares the scanner.
- The scrub of an upstream key echoed in a response now also replaces its JSON-escaped, percent-encoded and base64 (standard and URL-safe) forms, in headers, error bodies and streamed chunks, including when a form is split across chunks.

## 0.8.2

### Added

- A `[profile.release]` with thin LTO, one codegen unit and stripped symbols. Panics still unwind, which plugin panic isolation needs.
- `deny_private` also refuses 198.18.0.0/15, 192.0.0.0/24 and 6to4 and Teredo addresses that embed a private or local IPv4 address.

### Fixed

- A reload that adds an intercepted host now checks the running CA's name constraint for it, instead of failing when a client first connects.
- The failed-login count for an IPv6 address is kept per /64, so a host rotating through its prefix cannot reset it.
- The syslog exporter resolves the collector name on a helper thread with a timeout, so a hung resolver no longer stalls the log writer.

### Changed

- A plugin that masks data now fails closed. `Plugin::fails_closed()` (default `false`) lets a plugin say that a failure must stop the request, and the `placeholders` plugin returns `true`. Without an explicit `on_error`, such a plugin now rejects the request with a 503 when it skips or panics; before, the request went through unmasked. Set `on_error = "pass_through"` to restore the old behaviour; Reiki then warns at startup and on reload.

## 0.8.1

### Added

- `POST /keys/enable?key=<name>` (and under `/v1`) re-enables a key that was taken out of rotation, clearing its cooldown and disabled flag, and `reiki keys --enable <name>` calls it. It is in the OpenAPI document.
- An `[auth]` table (`backoff`, `max_failures`, `window_secs`) sets the failed-login limit. It applies to the client and admin listeners and to a reload without a restart.

### Security

- Response bodies are decoded by Reiki itself and capped at the per-upstream `max_decoded_bytes` (default 64 MiB) after decoding, so a small compressed body cannot expand without bound. A decoded, non-SSE response over the cap is buffered and answered with a 502 instead of streaming a truncated 200. SSE keeps streaming and ends with an error past the cap. Previously reqwest decoded on the standard transport and the 512 MiB check only covered the faithful transport.
- A stream's `rate_limit_per_minute` is enforced per selected-key attempt instead of once before dispatch. A failover, retry or fallback upstream now counts against the limit too, so a failing key cannot turn one allowed request into several upstream calls.
- Response bodies have a per-upstream idle timeout, `response_idle_timeout_secs` (default 300), and an optional total timeout, `response_timeout_secs` (unset by default). A stalled body no longer holds a client connection forever. A buffered decoded body times out with 504; a streamed body ends with an error.
- A POST or PATCH is no longer replayed after an ambiguous transport failure. A connect failure before the request was sent still permits a retry for any method, as does any explicit upstream error response. Idempotent methods (GET, HEAD, PUT, DELETE, OPTIONS, TRACE) keep their retry. An uncertain POST or PATCH now answers 502, or 504 on timeout.
- Failed admin logins are logged with the peer address (never the token) and counted for a per-address lockout, as on the client listener.
- `reiki hash-token` refuses a token shorter than 16 characters.
- The interception CA key is checked on load and reload: a key file that group or others can access is refused. Set `[tunnel.intercept] ca_key_permissions = "warn"` to load it with a warning instead, or `chmod 600` the key. `reiki ca-init` already writes it as 0600.

### Fixed

- The failed-login table no longer stops tracking new addresses when it is full of live entries: it drops the address with the fewest failures (oldest first), so a flood of addresses cannot leave a fresh one free to guess and a blocked address stays blocked.
- The placeholders plugin now treats Gemini-style `functionCall` and Bedrock-style `toolUse` bodies as tool calls, so credential placeholders stay masked in their arguments unless `restore_tool_args` is set.

## 0.8.0

### Changed

- The `gov` plugin is now `controlled` (crate `reiki-controlled`, feature `plugin-controlled`, docs `controlled.md`), since it looks for controlled-information markings and identifiers, not government data in general. `[plugins.gov]`, `reiki scan --plugin gov` and the `plugin-gov` feature still work. Findings now name the plugin `controlled`, so a dashboard or alert that matches `plugin="gov"` needs the new name.

### Added

- Plugins can report a finding with a `detail` label through `RequestCtx::report_detail`, for example a tool name or a lint rule. The host keeps only `[A-Za-z0-9_.:*/-]`, cuts the label at 64 bytes and drops it when empty. `Event::Finding` carries it as `detail`.
- The request log adds `detail` to a `finding` record when one is set. OCSF puts it in `finding_info.title` as `(kind:detail)`; `finding_info.types` stays the kind.
- `reiki_findings_total` ignores `detail`, so the label set does not grow with the names a plugin reports.
- `streams = ["a", "b"]` in a `[plugins.<name>]` table runs the plugin only for requests on those streams. Without it the plugin runs on every stream. An unknown stream name is a startup error. A reload that changes only `streams` applies live and keeps the plugin running with its counters.
- A reload swaps the interception CA when `ca_cert` or `ca_key` changes, or when a renewed CA file is seen by the watcher. The new CA must have a key that matches its certificate, a DNS name constraint covering every intercepted host, and no past expiry. A CA that fails a check refuses the whole reload and the old CA keeps serving.
- A swapped CA drops the certificates the old CA minted. New connections get certificates from the new CA, and open connections keep theirs. The reload report lists `intercept ca` in `changed`, and the log warns that clients must trust the new CA.
- `GET /status` shows the new CA's expiry after a swap.
- `requestlog` sends records to a syslog collector. `syslog = "udp://host:port"`, `"tcp://host:port"` or `"unix:///dev/log"` sends each record as an RFC 5424 message, with `syslog_facility` (default `local0`) and `syslog_app_name` (default `reiki`). A request never waits for the collector. A record only the collector misses is reported to the collector alone, once it is reachable again.

## 0.7.8

### Fixed

- An upstream key that a successful response echoes back, in a header or in the body, is now replaced before plugins and the client see it, including when it is split across streamed chunks. Before, only error responses were scrubbed. A server that echoes the key (a debug echo endpoint, for example) now returns it blanked, with the same length.

## 0.7.7

### Added

- `reiki keys` shows each key's upstream, state, cooldown, last status code and streams from the admin `/status`, as a table or with `--json`.
- `reiki keys --check` checks the configured key files offline for missing, unreadable, empty or group/world-readable files.
- `/status` key entries include `last_status_code`.
- Versioned admin API: all existing admin routes are available under `/v1/...` with the same auth; unversioned paths remain unchanged. `GET /v1/openapi.json` returns an OpenAPI 3.1 document describing v1 routes, bearer auth, `dry_run` on reload, and response shapes. Admin responses include `X-Reiki-Version`. The OpenAPI document is built from a static description in code to detect route drift. v1 gains only additive fields in the stability promise.
- Per-stream token usage reporting: Prometheus counter `reiki_tokens_total{stream,upstream,direction}` with direction `input` or `output`. Admin `GET /usage` returns JSON per stream and upstream with input, output and request counts since start. Token usage is extracted from OpenAI-style and Anthropic-style `usage` in non-streaming JSON bodies and from the last `data:` event of SSE streams.

## 0.7.6

### Fixed

- OCSF output maps `PATCH` to HTTP Activity id 9 and findings carry `finding_info.created_time`.

## 0.7.5

### Added

- Live reload. SIGHUP and the new `POST /reload` on the admin listener now apply plugin settings, the tunnel allowlist, `deny_private`, the intercept host table, `require_auth` and `leaf_ttl_hours` without a restart. A plugin whose table did not change keeps running with its counters, and requests already in flight finish on the plugins they started with. Listen addresses, the log file path and the CA files still need a restart and are reported as `restart_needed`. `/status` gains `config.generation`, `config.loaded_at` and `config.last_reload`, and `reiki check` summarises the tunnel policy.
- The request log records a `finding` for each detection hit, tied to its request by id, and `format = "ocsf"` writes the log as OCSF 1.3 events: HTTP Activity for requests and responses, Detection Finding for findings.
- `reiki reload [--dry-run]` and `POST /reload?dry_run=true` preview what a reload would change without applying or recording anything.
- `[reload] watch = true` reloads the config when the config file, a key file or a file a plugin reads has changed and stayed unchanged for one more `interval_secs` (default 5). It is off by default.
- A reload also rebuilds a plugin whose watched file changed: the placeholders `terms_file` is re-read when it is edited, without a config change.
- `reiki_findings_total{stream,plugin,kind,action}` counts what the detection plugins find, by kind and whether the request was blocked. Plugins report through a new `RequestCtx::report`, and the chain forwards each report to every plugin as `Event::Finding`.
- `reiki scan` runs the `phi`, `finance` and `gov` detectors over files or stdin and prints where they find identifiers, never the matched text. Exit code 1 means findings.

## 0.7.4

### Added

- A `gov` plugin that blocks or reports CUI, classification and dissemination markings, plus label-bound EIN, ITIN, CAGE, UEI, EDIPI and ECCN values, in outbound request bodies. See [docs/gov.md](docs/gov.md).
- A `finance` plugin that blocks or reports financial identifiers (IBAN, SWIFT/BIC, Bitcoin and Ethereum addresses, CUSIP, ISIN, card data next to CVV or track data, and header-bound account and SEDOL values) in outbound request bodies. See [docs/finance.md](docs/finance.md).
- A `reiki-detect` crate holds the scanning helpers the detection plugins share.

### Fixed

- The `phi` plugin scanned large tables and bodies of unclosed `INSERT` statements in time that grew with the square of their size, so one large request could hold the proxy for minutes in block mode. Scan time is now linear.

### Changed

- A bare CUSIP is reported only next to a `cusip` label or in a `cusip` column, because a random nine-character token passes the check digit one time in ten.

## 0.7.3

### Added

- The `phi` plugin reads tables: it maps header names and SQL column lists to identifier kinds and checks the cells under them, in CSV, TSV, pipe-delimited and Markdown text, SQL `INSERT` and `COPY` blocks. A new default-on `column` kind covers name, address, phone, zip and diagnosis columns.
- `pass_errors = true` on an upstream relays every error status as it is, so a 403 from a git server cannot take the key out of rotation.

## 0.7.2

### Added

- A `phi` plugin that blocks or reports PHI and NPPI identifiers (SSNs, card, routing, account, medical and provider numbers, ICD-10 codes, birth dates, and field values such as names and addresses) in outbound request bodies. See [docs/phi.md](docs/phi.md).
- `deny_private = true` in `[tunnel]` refuses private-range destinations, so a broad allowlist such as `*` reaches the internet and not the local network.

## 0.7.1

### Added

- `streaming = true` on `[upstream.X.forward]` relays a forwarder's answer as it arrives instead of buffering it (8 MiB cap), for socket and command forwarders alike. The request timeout then bounds only the wait for the response head.
- A forward upstream may serve an intercepted tunnel host, so intercepted traffic can leave through a forwarder such as a Bun or Node HTTP client.

- `confirm_quota = true` on an upstream re-sends a quota 429 once on the same key, after 3 seconds, before cooling the key.

### Security

- Credential placeholders stay masked inside tool-call arguments unless `restore_tool_args` is set.
- A failure from a request that started before a key rotation no longer disables the rotated key.
- Decoded response bodies are capped at 512 MiB.
- Failed logins are logged with the peer address, and an address with 10 failures in a minute gets 429 until the window passes.
- The request log masks credential query parameters and path tokens, whole credential header values, and JSON object keys.

### Changed

- Docs describe Reiki as an isolation proxy between an LLM harness and its providers, with key failover as one of its jobs.
- Term masking folds the body once instead of once per term.

## 0.7.0

### Added

- Per-upstream `[upstream.X.forward]` table hands each request to a `socket` listener or a `command`, both speaking plain HTTP/1.1. See [docs/providers.md](docs/providers.md#forwarding).
- `expect_uid` on a socket makes Reiki check the peer's uid before writing the key.
- `pass_env` for a command; otherwise its environment is cleared except `PATH`, `HOME`, `LANG` and `LC_ALL`.
- A failed link to a forwarder answers 502 without cooling the key, retrying, or using a fallback upstream.
- `reiki check` and startup warn about a missing or non-socket path, or a writable socket directory without `expect_uid`.
- `max_concurrent` on a forward `command` caps simultaneous runs (default 16); a request over the cap gets 502.
- `[tunnel.intercept] require_auth` (default true) requires the stream token on a CONNECT to an intercepted host served from a stream.

### Security

- CONNECT to an intercepted stream host requires the stream token (`require_auth`); this breaks intercepted setups that send no credential, which should put the token in the `HTTPS_PROXY` URL or set `require_auth = false`.
- One request disables at most one key, and a key-invalid marker must be a real JSON field.
- Dot segments and encoded separators in request paths are refused.
- A second `Connection` header, `Keep-Alive` and `Expect` can no longer reach the upstream, and headers named by `Connection` are removed.
- Echoed keys are redacted in relayed errors.
- Account-identifying response headers are not relayed.
- Client and admin listeners time out slow headers and bodies and cap connections.
- Tunnel head deadline, per-peer cap, 503 instead of silent drops, and a one-hour cap on opaque tunnels.
- `max_concurrent` caps forward command runs.
- The admin token must differ from stream tokens, and `passthrough` is a reserved stream name.
- The state file is written 0600 and exclusively.
- Release workflow builds run read-only with the release job isolated, and the toolchain and artifact actions are pinned.

## 0.6.0

### Changed

- Metrics are now the `metrics` plugin. `GET /metrics` on the admin listener needs `[plugins.metrics]` in the config (an empty table is enough) and returns 404 without it. A build that leaves the plugin out has no metrics code. Metric names and labels are unchanged. See [docs/monitoring.md](docs/monitoring.md).
- Upstream requests go through a `Transport` trait with two implementations, the reqwest one and the hyper one that keeps the client's request as sent. Behaviour on the wire is unchanged.

### Added

- Plugin API: `Plugin::on_event` receives request, failover, cooldown and rate-limit events; `Plugin::admin_paths` and `Plugin::on_admin` serve paths on the admin listener; `Plugin::uses_bodies` lets a plugin that never touches bodies leave response streaming (and `content-length`) alone. See [docs/plugins.md](docs/plugins.md).

## 0.5.0

### Changed

- Requests through `[tunnel.intercept]` are relayed with a hyper client that keeps the client's request as sent: header names keep their case and order, `Accept-Encoding` and `Connection: keep-alive` are forwarded, and only the credential, `Host` and hop-by-hop headers are edited. Previously they were rebuilt by reqwest, which lowercased and reordered headers and replaced `Accept-Encoding` with `identity`. See [docs/tunnel.md](docs/tunnel.md).
- Encoded responses on that path (gzip, deflate, brotli, zstd) are decoded before plugins and failover see them, and the client receives plain bytes.
- The per-upstream `http2`, `forward_accept_encoding`, `title_case_headers` and `send_keep_alive` settings apply to the reverse-proxy routes only.
- Reiki's TLS handshake toward the upstream is still its own; only the opaque tunnel keeps the client's.

## 0.4.3

### Added

- Upstream `title_case_headers` sends header names in Title-Case on HTTP/1.1, and `send_keep_alive` adds `Connection: keep-alive`. Both are off by default. See [docs/providers.md](docs/providers.md).

### Changed

- The upstream credential header is now sent first, ahead of the client's own headers. A client header with the same name as the credential header is still replaced.

## 0.4.2

### Added

- `[log]`: Reiki's own log can also go to a rotating file (`file`, `file_max_bytes`, `file_keep`), as text or JSON, with a `level` that is a plain level or a per-module filter. SIGHUP applies a changed level and reopens the file. `stderr = false` stops duplicating a service manager's journal. `RUST_LOG` still overrides the level at startup. See [docs/troubleshooting.md](docs/troubleshooting.md).
- Log lines made while a request is handled carry its id, which matches the `id` on that request's request-log records. Plugins can read it as `RequestCtx::id`.
- The key-failure warning reports `cooling_secs`, the cooldown the key entered.
- Intercepted requests refused for a `Host` mismatch or a path outside the API are logged at `warn`.
- Request log body shaping: `exclude`, `summarize` and `max_string_bytes` trim a JSON body before the size cap is applied, so a large request such as one carrying a long tool list stays parsed JSON instead of cut, escaped text. `body_excluded` names what was dropped.
- Event-stream response bodies are logged as parsed events or one assembled completion (`sse = "events" | "assembled" | "raw"`).
- `reiki ca-init --replace`; `reiki check`, startup and `GET /status` report how long the interception CA has left.

### Changed

- Event-stream response bodies in the request log are now an array of parsed events by default. Set `sse = "raw"` for the previous raw text.
- A 429 body that says "free usage limit" counts as quota exhaustion.

## 0.4.1

### Added

- `paths` on an intercepted host: path prefixes relayed to the host as the client sent them, for APIs that span several prefixes. Without it only the upstream's base path is relayed, as before.

## 0.4.0

### Added

- `[tunnel.intercept]`: Reiki terminates TLS for chosen tunnel destinations with certificates signed by a CA the client trusts, then relays each request through the normal route, so plugins, key handling, failover and rate limits apply to traffic that used to be opaque. Each host maps to an upstream and, unless that upstream is `passthrough`, a stream. See [docs/tunnel.md](docs/tunnel.md).
- `reiki ca-init`: creates a CA that is name-constrained to the hosts you list. `reiki check` loads the configured CA.
- `REIKI_PLUGINS` build argument: an ordered list that picks which plugins are registered and in what order, replacing the hard-coded list in `src/plugins.rs`. The plugin catalog now lives in `Cargo.toml`. See [docs/plugins.md](docs/plugins.md).

### Changed

- The tunnel configuration gains an optional `intercept` table; existing `[tunnel]` configs are unchanged.
- Provider names and setup moved out of the general docs into [docs/providers.md](docs/providers.md).

## 0.3.0

### Added

- The `placeholders` plugin, which swaps credential-shaped strings and a configured list of terms for stable placeholders before a request leaves and restores the originals in the response, including placeholders split across streamed events. The request log, which runs after it, records only masked traffic. See [docs/placeholders.md](docs/placeholders.md).
- Upstream `fallback`: when no key of an upstream is available or every key refuses the request, the request goes to another upstream with its model re-mapped. A `"*"` entry in `model_aliases` matches any model. See [docs/providers.md](docs/providers.md).
- Upstream `passthrough`: a plain reverse proxy mode that forwards the client's own credentials and headers untouched, needs no Reiki token, and uses no key from the chain. See [docs/providers.md](docs/providers.md).
- `[tunnel]`: a CONNECT-only forward proxy with a destination allowlist that copies bytes without reading them, so a client's own TLS session reaches the destination unchanged. Connections are logged with host, port, byte counts and duration. See [docs/tunnel.md](docs/tunnel.md).
- The `reiki-redact` crate, the credential scanner the request log and the placeholders plugin share.

- Plugins receive the request path (`RequestCtx::path`), the name of the key that produced a response (`ResponseHeadCtx::key`) and a new `on_attempt` hook after every upstream attempt, with the key's config name, the upstream status and, for an error response, the start of its body.
- The `requestlog` plugin records the request `path`, and on the response record the serving `key`, every `attempts` entry, and with `response = "body"` the response body and each failed attempt's error body (cut at `max_body_bytes`, masked, `body_truncated` when cut). Only key names are logged, never their values.

- Per-upstream `http2` and `forward_accept_encoding` settings. `http2` negotiates HTTP/2 over TLS; `forward_accept_encoding` sends the client's `Accept-Encoding` upstream and Reiki decodes gzip, deflate, brotli and zstd responses so failover can still read error bodies. Both default to off. The `forwarded` log line now carries the HTTP version.

### Changed

- Response hooks (`on_response_head`, `on_response_chunk`) now run in the reverse of plugin registration order, so the plugin that changed the request first sees the response last and can undo its change before the log or the client does. `on_request` and `on_attempt` keep registration order.
- Plugin API: `RequestCtx::new` takes the path and `ResponseHeadCtx::new` takes the key name. `PluginChain::on_request` takes a `RequestMeta` and `on_response_head` takes the key name.

## 0.2.0

### Added

- A build-time plugin API. The `reiki-plugin` crate defines hooks for the request, the response head and each response chunk; plugins are compiled in and configured with `[plugins.<name>]` tables. See `docs/plugins.md`.
- The `requestlog` plugin, which logs requests as JSON lines to a rotating file and to a live Unix socket, never logs credential headers and masks credential-shaped text. It is the `plugin-requestlog` feature, on by default and inert until `[plugins.requestlog]` is configured; build with `--no-default-features` to leave it out. See `docs/requestlog.md`.

### Changed

- A panic in a plugin hook is caught and logged instead of ending the request. A panic in `on_request` counts as a skip, so `on_error` applies.
- While a plugin is configured, responses are streamed without a `content-length` (HEAD, 204 and 304 keep theirs), so plugins can change body lengths and the final `is_last` chunk call follows the last chunk when the body ends normally.
- The request body is limited to 8 MB again after the plugins have run; a larger body is refused with 413.
- A SIGHUP reload checks the plugin tables, keeping the old config on an unknown plugin or a bad setting, and warns on every reload while the plugin settings differ from those the running plugins were built from.
- `PluginError` and the hook context types are `#[non_exhaustive]`; the contexts have `new` functions.
- `reiki check` lists the compiled-in plugins that are configured, as `N plugin(s)[names]`, and validates their settings without side effects. With no plugins configured its output is unchanged.
- Library users: `App::new` takes the plugin chain as a fourth argument, and `plugins` and `reiki_plugin` are new public items.

### Fixed

- Error bodies are valid JSON for any message. Quotes used to be replaced with apostrophes and backslashes or line breaks were not escaped.

## 0.1.2

### Added

- Optional admin listener with `GET /status` (per-key state, cooldown time left, per-stream chain) and `GET /metrics` in Prometheus format.
- The admin listen address must differ from the client one; startup and `reiki check` reject a match. The admin address is bound at startup, and a reload that changes it logs a warning.
- Optional per-stream rate limit, `rate_limit_per_minute`.
- Per-upstream `model_aliases` and `drop_params` body rewrites.
- Key health persisted next to the config as `<config>.state`, so cooldowns and out-of-rotation keys survive a restart.
- `quota_resets` per key, to end a quota cooldown at the provider's next reset instead of a fixed 5 h.
- `Retry-After` values given as HTTP dates are honoured.

### Fixed

- The admin listener accepts the `Bearer` scheme in any letter case, as HTTP requires.
- A SIGHUP reload no longer replays the state file, which could revive the disabled flag of a key whose secret had just been rotated.
- State writes are serialised, so an older snapshot cannot overwrite a newer one.
- A poisoned internal lock no longer turns every request into a 503.
- Plain `http://` upstreams are accepted only for the exact hosts `127.0.0.1`, `localhost` and `[::1]`, not for lookalike names.
- Restored cooldowns are capped at a year, so a hand-edited state file cannot overflow a timestamp.
- The declared minimum Rust version is now 1.99. It was 1.85, which the locked dependencies did not actually build on (they needed 1.88).

Earlier releases: see the GitHub releases page.
