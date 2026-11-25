# Changelog

## Unreleased

### Added

- The in-memory recent-key-failure ring now holds `failure_ring_size` entries (200 by default, `0` disables, 10,000 maximum) instead of a fixed 64, with reload applying a smaller cap by dropping the oldest records. Each row carries an `action` of `cooldown`, `rotate` or `pass-through`, and body snippets are capped at 200 bytes rather than characters. `GET /v1/failures` and `reiki failures` accept a `stream` filter, `reiki top` shows the action in local failure rows, and `/v1/metrics` appends `reiki_failure_ring_entries` and `reiki_failure_ring_dropped_total`. The query `limit` remains capped at 64. See [monitoring](docs/monitoring.md#get-v1failures) and [configuration](docs/configuration.md#top-level).
- Provider health contract: `docs/provider-health.md` and generated `docs/provider-health.openapi.json` describe the peer `/v1/health` contract (required/optional fields, units, nullability, unknown-field tolerance, enum extensibility, version negotiation). `host` and `sessions` are marked optional non-core extensions, off by default.
- Full-surface OpenAPI golden (`tests/golden/openapi-full-surface.json`) and `scripts/openapi-diff.py`, which reports `REMOVED`, `RETYPED`, and `NEWLY-REQUIRED` changes and exits non-zero on breaking changes. CI runs it as a warning (`continue-on-error: true`) until 1.0.

### Changed

- Public-repo hygiene: examples and tests use documentation IP ranges and neutral upstream names. Private literals remain only in the tunnel policy tests, each with a one-line comment explaining why. A tracked-file check rejects internal hostnames and domains, with justified exceptions in an allowlist, and runs in CI. No behavior, wire format or stored value changed.
- At startup, before binding listeners, Reiki probes the state directory (the parent of `<config>.state`) and, when `[log] file` is configured, the log directory by creating, writing a few KB to, and removing a uniquely named temp file in the same directory. With `[gateway] on_store_error = "closed"` (default) a state directory probe failure exits with a message naming the path and the OS error; with `"open"` Reiki logs one `WARN` and continues. A log directory probe failure never stops startup: Reiki logs one `WARN` and disables the log file for that run. No files are created outside the configured paths and no probe file is left behind. Operators with an unwritable state or log directory must fix permissions or free space, or set `[gateway] on_store_error = "open"` before upgrading. See [operations](docs/operations.md#startup-checks).
- Reader/wire documents published by peers or the admin API are now forward-compatible: unknown fields and unknown enum-like key states are ignored. Inbound mutation request bodies and config/state remain strict.
- Peer health OpenAPI now matches the wire document: response schemas allow unknown fields, `observed` is required, and enum-like fields are extensible. Host and sessions are described as optional non-core extensions.
- Admin OpenAPI now includes shared `Error`/`ErrorEnvelope` schemas and response/error content schemas for admin, stream-admin, and plugin routes.
- List-returning routes now have explicit bounds in OpenAPI and responses: `/v1/status` caps `keys`/`streams`/`observed` at 256 and reports `*_truncated`; `/v1/stream-console` caps `streams` at 256 and reports `streams_total`/`streams_truncated`, with a bounded 64-record audit tail; `/v1/streams` caps at 256 and reports `streams_total`/`streams_truncated`; client `/v1/usage` reports `streams_total`/`streams_truncated` with a 256-row cap.
- **Breaking:** `/v1/status` no longer publishes raw filesystem paths in `peers.tls_ca[].file`; entries now carry only `ca_not_after` and `ca_days_left`.
- **Breaking:** peer listener refusals (401/429/404/503) now return the shared JSON error envelope (`{"ok":false,"error":{"code","message"}}`) instead of empty or plain-text bodies.

## 0.15.1

### Added

- `reiki top` sorts the Streams table by stream name, usage, chain, request rate or expiry with the matrix keys `o`, `O` and `r`; past the last column the table returns to the default usage order. The sorted header shows `▲` or `▼`, unavailable values sort last in both directions, and equal values keep the default order. The Streams key list and hints name the new keys. See [top](docs/top.md#streams).

### Fixed

- A failed state write no longer latches the gateway closed until restart. The state writer probes the current counters every 5 to 30 seconds while the store is failed and reopens admission on the first successful write; close and recovery each log once, and `reiki_gateway_store_ok` exposes the state on `/v1/metrics`. Alert and troubleshooting guidance is in [monitoring](docs/monitoring.md#alert-rules) and [operations](docs/operations.md#troubleshoot-gateway-store-unavailable).
- The `reiki top` Streams selection and detail pane follow a stream by name instead of by row index, so a refresh or re-sort no longer highlights and describes a different stream.

## 0.15.0

The opt-in features introduced in 0.14.0 — the multi-stream gateway with admission limits, dynamic streams with a hash-chained audit log, the `plugin-wasm` runtime host and `plugin-cost` estimates — now have operator documentation that is checked against the binary. Their behavior is unchanged here; see the 0.14.0 entries below for the original feature and behavior changes. `Cargo.toml` is intentionally still at 0.14.0.

### Added

- Dynamic stream runbook: create, show, rotate and revoke tokens, expiry and TTL ceilings, assignable keys and limits, offline audit verification, and backup/restore of the state file, JSONL audit log and its `audit.head` sidecar. Every command is checked against the CLI definitions. See [dynamic streams](docs/dynamic-streams.md).
- Install and operations pages covering the tuishou one-liner's status, the manual install path that works today, release verification, upgrade and rollback, exact failure messages, logs, health checks, and the mode and location of key, state and audit files.
- A checked quickstart in the README with expected `check`, key-table and proof output, plus a documentation test that replays the wrong-token proof and the `keys` and `failures` commands against a throwaway instance.
- `make docs-check`, `tests/docs_examples.rs` and a CI job that validate documented TOML with `reiki check`, CLI flags with help output, config keys with the docs, local links and anchors, and the README proof.
- `AGENTS.md` with commands, repo map, conventions and stable exit codes, and `llms.txt` as a short docs index.
- Automatic dev pre-releases: a successful CI push to `main` tags `vX.Y.(Z+1)-dev.<commits since last stable tag>.g<short hash>`, publishes it by dispatching the release workflow on the dev tag (so the signing identity is `release.yml@refs/tags/<tag>`) with the same archives, checksums, SBOM and attestations as a stable release, and keeps only the ten newest dev pre-releases. Docs-only pushes, a moved `main` and existing tags are skipped; stable tags must still match `Cargo.toml`.
- `scripts/set-release-version.py` and unit tests: stable tags are refused when they do not match the checked-in version, dev tags rewrite the checkout only, and version ordering places a dev build after its base stable and before the next stable.
- A final-response plugin phase for security checks that must inspect the body after placeholder restoration. It adds `Plugin::needs_final_response` and `Plugin::on_final_response` to the native plugin API; the host buffers the body only for plugins that opt in and fails closed when the buffer bound is exceeded.

### Changed

- The README and docs follow one template: proof-first quickstart, how it works, configuration, operating, security model, compatibility matrix, limits and non-goals, development and links. The docs index lists every page, including the new install, operations and dynamic stream pages.
- Upgrade notes spell out the new keys and defaults, fail-closed behavior, the state directory beside `<config>.state`, mode `0600` files, and the audit log and sidecar names.
- Stream administration is cross-linked from configuration, CLI, keys and monitoring docs; static and dynamic stream controls are described with the same vocabulary.
- `REIKI_PLUGINS` now rejects a build order that puts `toolpolicy` before `placeholders`; the late security check must see restored tool arguments. No config, CLI flag or config-key shape changed.
- `toolpolicy` canonicalizes JSON argument objects before applying host and tripwire rules, so escaped spellings of the same value cannot evade a check. Raw arguments still reach body rewriting and the journal.

### Added

- `reiki top` sorts the peer matrix by column: `o` and `O` step through the peer name, each provider (by requests in the last hour), version, load, disk and sessions, and back to the default order; `r` reverses the direction. The sorted header shows `▲` or `▼`, `ALL` stays first, and the cursor follows its peer through re-sorts and refreshes. The key list in `?` names the new keys.
- `reiki top` gains a read-only Streams screen (`t`) backed by the new ordinary-admin `GET /v1/stream-console` route. It shows static and dynamic streams sorted by usage, chains, limits, current rate/concurrency, expiries, last use and state, with token ids and a bounded redacted audit tail in the detail pane. It reuses `REIKI_ADMIN_TOKEN` and `[admin] listen`, never reads the stream-management token, says when `[stream_admin]` is disabled, keeps the last good answer on refresh failure with backoff, and offers no mutation keys. See [top](docs/top.md#streams), [monitoring](docs/monitoring.md#get-v1stream-console) and [keys](docs/keys.md#dynamic-stream-credentials).

### Fixed

- Wasm host input bounds: guest strings are capped before sanitization, `log` rejects oversized template ids and argument vectors, `request_state.get` rejects oversized keys, and finding-kind interning is isolated per plugin inside the bounded global table. All violations poison or degrade fail-closed.
- Documentation: corrected key-file permission and length rules, listener collision and reload-watch rules, key JSON `scopes`, failure classes, `reiki plugin inspect`, file paths and version strings.
- Config validation now treats a wildcard client listener and a loopback admin or tunnel listener on the same port as a collision, instead of only comparing the strings.
- Corrected stale documentation links and version strings, and clarified that a failed audit append refuses the management mutation instead of silently continuing.
- A decoded, buffered success no longer replaces its gateway token estimate with zero; the permit is dropped so the estimate stays charged while the provider's `usage` object remains ignored for compressed bodies.
- Gateway completions keep the estimate in the window where the request was admitted, instead of subtracting it from a minute, day or month that rolled over in flight.

### Security

- Documentation only: spell out that stream management uses a dedicated token with optional two-party confirmation, that state and audit files are mode `0600` and hold hashes rather than raw tokens, and that a failed audit write fails closed.
- Clarify the trust boundary: gateway counter failures refuse with `503` under the default `[gateway] on_store_error = "closed"`, native plugins are trusted in-process code, and runtime Wasm plugins are sandboxed behind `plugin-wasm`. This clarification changes no behavior; the items below do.
- Builtin credential patterns are also matched against decoded JSON string values, so escaped prefixes and separators cannot reach the model unmasked. Escaped matches are restored by their decoded value.
- `toolpolicy` now observes the final restored response body, closing the gap where a configured term equal to a forbidden host was checked in masked form. The existing `requestlog`, metrics and wasm slot ordering is unchanged.

### Changed

- Wasm embedded manifests now reject log and reject template keys longer than 256 bytes at load time, matching the host call-time bound; invalid manifest errors no longer echo the key. No CLI or public API change.

### Tests

- Added Wasm host input-bound, template-key, and per-plugin interning regression tests.
- Added config validation edge cases for token hashes, duplicates, rate limits, chains, reserved names, admin credentials, listener collisions and auth bounds.
- Added documentation checks for config snippets, new keys and flags, local links and anchors, and the README proof against a live throwaway instance.

### Admin API and top

- Optional read-only admin token: `[admin] read_token_sha256`. It opens the read routes (status, failures, stream console, metrics, OpenAPI and plugin GET routes) and gets `403` on `POST /v1/reload` and `POST /v1/keys/enable`. Startup refuses a hash equal to the admin token or to any other credential. See [monitoring](docs/monitoring.md#read-only-token).
- Read-only callers get no free text: `failures[].note` is `null`, `last_reload.error` reads `reload_refused`, `stream_admin.state_error` reads `state_unavailable` and audit `reason` is `null`.
- Audit event `reiki::admin_audit` for reload, dry-run reload and key enable, with `action`, `token_class`, `actor` and `outcome`. The actor comes from an optional `X-Reiki-Actor` header, cut to 64 characters and limited to `A-Z a-z 0-9 . _ - @ :`; it never affects authorization.
- `reiki top` reads `REIKI_ADMIN_READ_TOKEN`, preferred over `REIKI_ADMIN_TOKEN` when both are set.

- Every admin-listener refusal uses one envelope, `{"ok":false,"error":{"code","message"}}`. Stream-management errors keep `error.message` and replace `error.type` with `error.code`. A `401` from the stream routes now has this body instead of an empty one. Reload's `422` carries `config_refused` and keys' `404` drops its echoed `key` field.

## 0.14.0
### Added

- Opt-in `[stream_admin]` dynamic stream management on the admin listener: a dedicated bearer token, `/v1/streams` create/read/update/delete and token/revoke routes, strict JSON with `Idempotency-Key` and reasons, optional two-party confirmation, assignable keys and ceilings, a fail-closed hash-chained JSONL audit log with an `audit.head` tail sidecar, and an atomic mode-0600 state file. `reiki streams` adds list, show, create, update, revoke, token add/revoke and offline audit verify/head commands. New metrics, OpenAPI paths and status fields appear only when the feature is configured; peer health publishes stream counts for `reiki top` with `expose_usage`. See [configuration](docs/configuration.md#stream_admin), [keys](docs/keys.md#dynamic-stream-credentials), [monitoring](docs/monitoring.md#stream-administration-routes) and the [API list](docs/README.md#stream-admin-api).
- `plugin-wasm` (off by default): runtime Wasm component plugins in the D5 inner slot. Phase 1 supports `on-request`, default-allowed capabilities, gated `request-body:read|edit` and `finding:detail-vocab`, embedded manifest pinning (`sha256` + `reiki-manifest`), host-applied JSON edit ops, limits/fuel/epoch traps, reload narrowing, `reiki plugin inspect`, and host-enforced request-side tool allowlists that reuse `reiki-toolpolicy` request parsing. See [Wasm plugins](docs/wasm-plugins.md).
- Passthrough answers are observed in memory and published as a strict `observed` array in `/v1/status` and peer health. Each `(upstream, account)` entry carries a fixed status class, remaining `Retry-After`, quota headers and a host timestamp; no credential value is read or published. `[upstream.NAME] account` labels these entries and defaults to `default`.
- `GET /v1/metrics` gains `reiki_passthrough_requests_total{upstream,status_class}`, `reiki_observed_cooldown_seconds{upstream,class}` and `reiki_upstream_unaccounted_1h{upstream}`. `reiki top` shows observed entries as display-only `observed/{account}` columns.
- `[peers] allowed_urls` pins `[peers.NAME]` read targets to an exact base-URL allowlist; empty keeps https-or-loopback. Peer CA expiry is reported as `peers.tls_ca` on `/v1/status`, with warnings for unreadable, non-certificate and near-expiry CA files.
- Response-header parsers are wired into observed quota fields for generic `x-ratelimit-*`, OpenAI-style and Anthropic rate-limit headers.
- Two small reference component fixtures (`crates/reiki-wasm/tests/fixtures/no-grant.wasm` and `body-edit.wasm`), an end-to-end conformance suite, and a CLI test for `reiki plugin inspect` text and JSON output. Each fixture has a recorded SHA-256 in `SHA256SUMS`; the rebuild test compares freshly built bytes when the `wasm32-wasip1`, `wasm-tools`, and `perl` toolchain is available and skips clearly otherwise. The examples build with plain `cargo build --release --target wasm32-wasip1` and `wasm-tools`, with no `cargo-component`.

### Changed

- `plugin-wasm` now pins `wasmtime` and `wasmtime-wasi` to 51.0.0. The previous 42.0.2 pins are affected by published RUSTSEC advisories. The host API used by Phase 1 is unchanged in 51.0.0. The component loader now refuses any import of `wasi:filesystem`, `wasi:sockets`, or `wasi:http` at load time; those interfaces cannot be granted in Phase 1. Two new reference fixtures and end-to-end tests prove that a reference plugin cannot open, truncate, rename, or hard-link a file through WASI, and that a component importing WASI filesystem or sockets is refused before execution.
- Multi-user gateway foundations: `[streams.NAME.limits]` adds per-stream `rpm`, `tpm`, `concurrent`, daily and monthly request/token budgets with configurable `period_zone`; `allowed_upstreams` and `allowed_models` glob allowlists; and weighted fair sharing of contested keys via `[streams.NAME].weight`. `[gateway] on_store_error = "closed"|"open"` (default `closed`) controls fail-closed counter availability. All refusals use fixed codes and messages, with no token, model, body or key value in the response, logs, metrics labels or events.
- Self-service `GET /v1/usage` on the client listener lets a stream see only its own request and token totals, with its own `[gateway] usage_rpm` budget. `Event::Usage` emits final token counts at body settlement; `GatewayRejected`, `GatewayStoreError`, `reiki_gateway_rejections_total` and `reiki_gateway_store_errors_total` expose refusals and store failures.
- Gateway counters persist atomically in `<config>.state` alongside key health. A changed stream token starts fresh on reload while an unchanged one keeps its counters. Peer health publishes aggregate `gateway` counts only when `expose_usage` is on, and `reiki top` shows them on peer cards and in the `ALL` detail.
- `plugin-cost` (off by default): `[plugins.cost] prices` maps upstream names to currency-labelled model price rules with exact-then-longest-glob matching, optional cached-input pricing and no guessed prices. Fixed-point micro-unit estimates are recorded by UTC hour/day/month in a bounded mode-0600 state file. Per-stream `budget_monthly`/`budget_daily` with `on_exceed = "report"|"block"` reuses the gateway 429 refusal path. Admin `GET /v1/cost` with period/stream/upstream filters, JSON/CSV `reiki cost export`, bounded Prometheus series, a self-service `cost` block on the caller's own `/v1/usage` row, a `/v1/status` fragment and a local-only `reiki top` spend line report estimates from the operator table, never provider invoices. See [cost](docs/cost.md), [configuration](docs/configuration.md#pluginsname) and [monitoring](docs/monitoring.md#get-v1cost-and-get-v1costmetrics).

### Changed

- Stream admission replaced `rate_limit_per_minute` with `[streams.NAME.limits]`; the old key is now an unknown-key error, with no compatibility shim. A request is admitted once before key selection, so failover and fallback attempts no longer spend a second stream request. `/v1/status` streams now carry `limits` and `usage` objects instead of `rate_limit_per_minute`.
- `days_from_civil` moved into `reiki-plugin::timefmt` alongside `civil_from_days`; host modules now import the shared helper.
- `reiki_health::Health` now requires `observed`; older peer documents without it are refused. Client-supplied `X-Reiki-*` request headers are stripped before every upstream transport.
- `Event::Usage` now carries the declared model, operator key name, input/output/cached-input token counts and a bounded status class. Plugins that pattern-match it must use a wildcard arm. The plugin API also adds static budget refusals, metrics text, cost fragments for the authenticated stream's own usage row, `AdminCtx::query` and a private `PluginHost` state path.

### Fixed

- Wasm components generated by `wit-bindgen` import `reiki:plugin/types@1.0.0` and a bare `request` type. The host now accepts those generated non-capability imports instead of refusing to load a valid component; other unknown imports remain errors.
- A `request-body:edit` component that also reads the body can declare `request-body-read = true` alongside its edit request. The manifest and import-derived capability checks now agree, so an edit plugin loads with both body grants instead of failing on a manifest mismatch.
- A failed `memory.grow` now traps immediately and the host searches Wasmtime's whole error cause chain when classifying traps. A plugin over its memory limit is reported as a memory-limit failure and fails closed instead of surfacing as a generic trap.
- The Wasm deadline is rechecked after host calls and poisons the store when it has passed, so repeated host calls cannot outlive `deadline-ms`.
- Peers that read another instance's health document now accept `capacity_group` in polling snapshots and key events, so a configured group no longer breaks the peer view. Unsafe group values are still refused on the wire.
- A peer URL that points at this instance's literal client, admin, tunnel or peer listener is refused at config load and reload; names are never resolved. Invalid base URLs, including ones with embedded credentials, are reported without echoing the URL.
- `reiki keys` accepts the new `observed` and `peers` fragments in `/v1/status`, so key tables and JSON output no longer fail on the status document published by this version.
- Error bodies read for observed passthrough classification are replayed intact to the client; a body read error is reported to the observer and the client still gets the bytes read so far.
- A CONNECT connection still waiting for its request head is closed at the shutdown drain cut instead of holding a socket until the head timeout.
- Tunnel idle detection uses one monotonic clock, so a wall-clock step cannot keep an idle tunnel open or close an active one. Tunnels cut by shutdown now log the `bytes_up` and `bytes_down` that actually moved.
- Plugin request hooks offloaded to the blocking pool keep the `req{id=...}` span in their log lines. A session whose blocking hook is cancelled now fails closed with 503 if it is reused, instead of running without its plugin state.
- Documentation: deduplicated `docs/README.md` navigation, removed the broken `service.md` link, listed every docs page, and corrected stale key-permission text in quickstart, limits and security docs. Monitoring admin-route wording now matches the `/v1`-only route mounting.

## 0.13.0

### Changed

- Quota classification no longer treats vendor-specific body phrases as built-ins. `subscribe to go` and `free-models-per-day` must now be supplied in `[upstream.NAME] quota_phrases`; the generic 402, credit balance, quota and insufficient rules are unchanged.
- `limits.preset` names are documented as optional named examples rather than authoritative provider caps. Preset lookup is an opaque data-table lookup: no provider name, response body or header chooses a preset.
- `requestlog` now records summaries only by default. `body` defaults to `summary`, and a new `capture_bodies` option defaults to `false`; no prompt, response or tool-argument text is written to the file, socket or syslog unless body capture is explicitly enabled. The summary still records method, route, status, durations, byte counts, token-setting counts, key names, attempts and failure findings.
- Body capture requires `capture_bodies = true`: `body = "full"` and `response = "body"` are config errors without it, and enabling it logs a startup warning. Every captured request, response and failed-attempt body is masked and capped by `max_capture_bytes` and `max_body_bytes`, and the live socket feed follows the same rules as the file and syslog sinks.
- Security docs now spell out the agent-framework supply-chain scenario: bearer stream tokens have no source binding or built-in expiry, what they buy on the client and intercepted-tunnel listeners, why the global tunnel allow-list is not a per-stream control, and why body masking is not a general cloud-token detector. A regression test confirms that a valid token for another stream cannot authorise an intercepted host.
- The workspace, fuzz and helper crates now build with Rust edition 2024. The edition migration adds no unsafe code; a test that needed `std::env::set_var` now runs in a child process.
- Request plugin hooks, including placeholder masking of large bodies, run on Tokio's blocking pool. A CPU-heavy hook no longer stalls health checks, watchdogs, admin requests or other connections; a panic that escapes the chain fails the request closed with 503.
### Added

- `[upstream.NAME] capacity_group` is an optional, opaque operator-set label. It appears on that upstream's `/v1/health` entry and in `reiki top`; it is never inferred from key labels, upstream names or responses, and unsafe values are refused at config load and on the wire.
- Health document wire types now live in the small `reiki-health` crate shared by the publisher and `reiki top`. The JSON shape is unchanged, with strict parsing and round-trip tests in the new crate.
- `requestlog` gains a summary-only export for log collectors at `[plugins.requestlog.export]`: rotating JSON Lines or RFC 5424 syslog over UDP/Unix datagram, with a bounded non-blocking queue and an allow-listed `request_summary` field set. Drops are counted as `reiki_requestlog_export_dropped_total`, exposed at `GET /v1/requestlog/metrics`, in `GET /v1/status` and in `reiki top`. See [summary export](docs/requestlog.md#summary-export).
- New `toolpolicy` plugin inspects tool calls in OpenAI Chat and Anthropic Messages responses, in whole bodies and streams. It checks declared tools, user/system host provenance and command tripwires; `report` is the default and `block` replaces flagged calls with a plain-text notice. See [tool policy](docs/toolpolicy.md).
- `toolpolicy` writes a rotating owner-only JSONL journal and serves counters at `GET /v1/toolpolicy/metrics` on the admin listener.
- A quota cooldown with a known reset now carries that absolute instant to peer consumers as `quota_reset_at` on the labelled key view in `/v1/health`, with the peer OpenAPI schema and `reiki top` wire reader updated. Daily free tiers need a `quota_resets` schedule for the reset to be known; the fixed window publishes none.
- `[tunnel] close_idle_on_stop` (default `true`) closes tunnels with no bytes in either direction for at least one second as soon as a stop begins. Active tunnels get the configured drain and are then cut; `false` keeps every tunnel until the drain ends. The change is live on reload for new tunnels.
### Added

- Cargo-fuzz targets for the SSE/stream parser, scrubbing and redaction, JSON key-invalid detection, config and key-file parsing, percent-decoding and path normalisation, tunnel allow-lists and header parsers. Each target checks for panics, leaked secrets and unbounded output, with hand-written seed corpora and a weekly/manual CI workflow that uploads crash artifacts.
- A `fuzzing` feature that exposes the parser internals the harnesses need without adding `unsafe` code or changing default builds.

### Fixed

- A malformed CA private key can no longer echo its PEM header text into `reiki` startup errors: key parsing reports a fixed description, and short-token and key-file errors likewise print fixed descriptions (with the configured key name) instead of data derived from the secret.
- Terms in the `placeholders` plugin now match Unicode spellings whose lowercase mapping expands to several characters. A term written as `i` plus a combining dot is masked by `İ`, whose lowercase mapping is two characters, and a match inside the expansion masks the whole original character.
- The stream scrubber finds a partial secret at a chunk boundary with one reverse-trie walk per byte instead of scanning every form of every key. High-throughput streaming with many configured keys no longer pays a per-form cost on each chunk.
- Request paths are checked for dot segments and encoded separators through repeated percent-decoding, up to eight layers. Double-encoded forms such as `%252e%252e` are refused, and malformed escapes or undecodable bytes are refused rather than forwarded.
- `/status` parses its `config`, `intercept` and `log` fragments as JSON before appending them in place of the closing brace, preserving field order. Trailing whitespace in an assembled fragment can no longer corrupt the response; an unmergeable document now answers 500 instead of invalid JSON.
- Unknown admin paths answer `404` for every HTTP method, not only `GET`.
- The peer publisher publishes the running listener's mode and its TLS certificate under one lock. A reload can no longer observe a TLS listener whose certificate has not been installed yet.
### Removed

- Old key state-file formats: only the current `keys` + optional `shaper` TOML shape is read. Files in another format are ignored with a log line and rebuilt. Rotation now requires the saved key digest and upstream, and no longer falls back to file mtime.
- Config aliases and legacy keys kept for older versions: removed the `gov` compatibility shim, the `REMOVED_PLUGINS` list, and the ignored-key warning plumbing. `Config::parse` now parses and validates directly; unknown or renamed config keys are errors and only the current names remain.
- Unversioned admin routes. `/status`, `/failures`, `/reload`, `/keys/enable` and `/openapi.json` are gone; the admin API is `/v1/*` only, and plugin admin paths must be requested under `/v1`.
- Compatibility fields in `/v1/status` and `reiki keys`: peers no longer publish or read the older peer-health and failure-ring shapes. `scopes` is always emitted, including `[]`.
- Older peer wire formats: the health document, upstream entries and key events now require the current fields and reject unknown ones. Removed the `observed_at` age fallback and the older top-view text grid.
- `reiki top` data-layer compatibility with older publishers: strict current health shape with version validation; no older-publisher markers or text-grid parsing.
- Legacy documentation: removed `docs/service.md`, all `Since 0.x` version qualifiers and compatibility/migration notes from configuration and upgrade docs, and updated every admin route, anchor and example to the current `/v1/*` shape.

## 0.12.0

### Added

- Test coverage re-verifies the state-file recovery path: huge, negative, NaN-like and zero recovery and saved-at values are rejected or capped before any `Instant`/`SystemTime` arithmetic.
- Test coverage re-verifies the request-body timeout: a client that sends complete headers and then stalls on a `Content-Length` or chunked body is cut off with 408 on the real client listener.
- `reiki top` gains an `ALL` fleet row in the matrix and tabs. It is selected by default with more than one peer, sums fresh peers' request gauges per provider, carries the worst key state, and opens a detail with per-upstream totals and per-peer `(peer, upstream, requests, limited, state)` tuples on Enter. `a` returns to it.
- The `reiki top` Requests chart now plots one coloured line per upstream, including credential upstreams, with a clipped legend, real interval-based axes and a rounded zero-based maximum. `s` toggles overlaid lines and a stacked total.

### Changed

- Behaviour change: `[admin].listen` must be a loopback address (when given as an IP:port). The admin listener has no TLS option, so a non-loopback bind is now a config error; terminate TLS in front and bind on loopback if the admin routes must be reachable off-host.
- In `reiki top`, `u` also cycles the Requests chart series (requests, limited, tokens), and stale peers are excluded from fleet charts with the fresh count shown in the title. The X axis is labelled from the sample interval and point count, not raw sample indices.
- Top-level `strict_key_permissions` (default `true`) rejects key files that are readable by group or others. Set it to `false` to keep the old warning-only behaviour.

### Changed

- Behaviour change: `[admin].listen` must be a literal loopback IP and port. The admin listener has no TLS option, so a non-loopback bind is now a config error; hostnames such as `localhost:9090` are rejected too, even if they would resolve to loopback. Terminate TLS in front and bind on loopback if the admin routes must be reachable off-host.

### Fixed

- Login throttling on the client, admin and peer listeners now checks the lockout and reserves the attempt under one lock. A concurrent burst of bad tokens can no longer all pass the check before any failure is counted; a successful peer poll still leaves earlier failures counted.
- Plugin panics in `new_state` and `on_shutdown` are caught and logged like the other hooks. A panicking `new_state` now fails the request closed with 503 when the plugin's resolved `on_error` is `reject` (the default for masking and block-mode plugins); opting in with `on_error = "pass_through"` keeps the old default-state behaviour. A panic in a response hook also blocks the request: buffered responses are refused and streamed bodies stop at 503. A panicking `on_shutdown` no longer stops later plugins from shutting down.
- Placeholders no longer skip a long JSON string because it exceeds a decode-size cap; masked terms inside very long escaped strings are redacted. A missing or wrong per-request placeholder state now fails closed with 503 instead of passing the request through unmasked.
- The startup state-directory probe uses an unpredictable file name with `create_new` and `O_NOFOLLOW`. A pre-planted file or a symlink at the probe path can no longer make the check pass or redirect the write.
- The request-log sink refuses a path that is a symlink or not a regular file (`O_NOFOLLOW` plus an `fstat` check). A planted symlink can no longer redirect log writes to another file.
- Key file permissions are checked from the same file handle used to read the key. With `strict_key_permissions = true` (the default), a group- or world-readable key file is a load error; reloads keep the last good key table rather than accepting a loosened file.
### Fixed

- An intercepted CONNECT now keeps the exact authorised host entry (upstream and stream) for its lifetime. A reload between authorisation and `serve` can no longer switch an already-authorised tunnel to a different upstream or stream; new CONNECTs use the reloaded table.
- A response that still carries a content coding Reiki did not decode (an unknown name or a list such as `gzip, br`) is refused with 502 when a key is in play, instead of relaying raw bytes that could contain an echoed key. Responses without a key in play, bodyless responses, and single recognised codings are unaffected.
- The scrubber reserves half of each key's 4096-form budget for the raw form and its split variants, with space-separated forms first. A long key with a space inserted at any boundary is now redacted instead of being crowded out by encoding variants.
- `GET /status` gains `log.write_errors`, the number of service-log file writes dropped since start. A failed write now warns on stderr at most once a minute instead of only once ever.
- A request body with more than 4096 top-level members is now refused with `400 too_many_members` before any key is used, instead of being rewritten by a scan that `request_model` does not complete.

### Fixed

- `reiki peers --watch` no longer grows a watched document without bound: key events that would add a 257th upstream are dropped and counted in a warning.
- A reload that changes `leaf_ttl_hours` now clears the cached leaves, so new intercepted connections use the new lifetime instead of reusing a leaf minted under the old TTL.
- A configured key over 4096 bytes is now refused at load instead of building unbounded scrubber forms. Form generation is lazy and bounded per category, so a long key cannot crowd out the raw and split forms that matter most.
- A streaming forwarder body is now capped and time-bounded: `[upstream.NAME.forward]` gains `max_stream_bytes` (default 64 MiB) and `stream_timeout_secs` (default 1800), and a stream past either limit ends instead of running without bound.
- `reiki peers --watch` no longer accepts an unbounded SSE feed: at most 1024 items are kept per feed and each stream chunk is cut at 64 KiB.
- The peer publisher now tracks whether its listener is plain HTTP or HTTPS on every start. A reload that switches a peer token from a plain to a TLS listener (or back) no longer keeps the old scheme's state.
- `POST /keys/enable` now parses `?key=` strictly: a repeated, bare or unknown flag, or invalid percent-encoding, answers `400` instead of enabling an unintended key.
- The request log now refuses any configured header whose name contains an authentication credential pattern (`auth`, `token`, `key`, `secret`, `cookie`, `credential`), not just a fixed list of names.

### Fixed

- `strict_key_permissions` now rejects key files with group or other **write** bits as well as read bits (for example mode 0620 or 0666). `reiki keys --check` reports the same verdict, and the error and warning messages name the read/write check.
- A peer SSE event's accumulated `data` is now bounded after UTF-8 decoding. Invalid bytes could make the decoded value longer than the raw line, letting one event exceed the configured cap.
- Intercepted `CONNECT` requests now count towards the shared `[auth]` login throttle. Repeated bad `Proxy-Authorization` headers get `429` with `Retry-After` after `max_failures`; a valid token clears the address's count. Addresses whose peer cannot be read are refused.
- Accepting a new request-log socket client no longer sends empty pings to existing subscribers. A subscriber with a full bounded queue is no longer disconnected by unrelated connections; only broadcasts prune subscribers that cannot keep up (or have disconnected).
- The 400 invalid-key marker is now read from parsed JSON string properties instead of substring matching. A plain-text or echoed copy of `"reason": "api_key_invalid"` no longer takes a healthy key out of rotation.
- The request-log file's descriptor no longer keeps `O_NONBLOCK` after the regular-file check. `O_NONBLOCK` is only used to avoid hanging while opening a FIFO that is then refused; writes to a regular file on a network filesystem can no longer fail with `EAGAIN`.
- Re-verified that scrubber forms stay sorted longest-first across all configured secrets, so a short form from another key cannot be blanked before a longer form that contains it.

## 0.11.0

### Fixed

- A 402 that says the request asks for more tokens than the account can afford (`fewer max_tokens` or `can only afford`) is now `request_error`: the proxy tries the next key without cooling this one, records each occurrence in the failure ring as `request_error` with `cooled: false`, and returns the last upstream 402 unchanged when every key refuses. The phrases are read from JSON `error.message` or `message`, with the whole-body substring check kept only for non-JSON bodies.
- The scrubber now also covers JSON `\uXXXX` escapes of letters and digits, percent-encoding of unreserved characters, base64 at the other two byte alignments with partial edge groups trimmed, and keys split by ASCII whitespace or control characters. Failure-ring snippets also check a compacted copy of the text and replace the whole snippet with `[body withheld: contains key material]` if key material remains.

### Added

- Per-upstream `request_error_phrases`, `quota_phrases` and `rate_limit_phrases` extend the built-in response body checks. Request-error phrases pass a 400, 402 or 413 through; quota phrases cool the whole key; rate-limit phrases cool the route and model. Lists are case-insensitive and validated on load.
- Startup probes the state directory (parent of `<config>.state`) by creating and removing a file before binding listeners. A failure exits 1 with an error naming the directory and the OS cause.
- Panics are logged at error through `tracing` with message and file:line:col, then passed to the previous hook, so unwinding and the default stderr report are unchanged.

### Removed

- The `plugin-gov` feature, `[plugins.gov]` config table, `reiki scan --plugin gov` alias and `REIKI_PLUGINS=gov`. Use `plugin-controlled`, `[plugins.controlled]` and `controlled`; the old forms fail with an error naming the replacement.

## 0.10.6

### Changed

- The columnar scanner shared by the PHI and finance plugins lives once in `reiki-detect` (about 360 duplicate lines removed).

### Fixed

- An OpenRouter `free-models-per-day` 429 is a quota exhaustion and cools the whole key, not a short rate limit; the free tier allows only a small daily number of requests per account across every free model.

## 0.10.5

### Added

- A bounded in-memory ring of the last 64 key failures, shared across streams. Each entry keeps Unix seconds, stream, key, status, class, `Retry-After` as received, whether and for how long the key cooled, and a printable body snippet capped at 200 characters with key material scrubbed out. Recording never waits on the lock, and the ring is never written to disk.
- `GET /failures` and `GET /v1/failures` on the admin listener, newest first, with optional `?limit=1..64` and `?key=NAME`, plus its OpenAPI description.
- `reiki failures [--limit N] [--key NAME] [--json]`, a table cut to `COLUMNS` or the endpoint's JSON array.
- The peer health document carries failure counts by class over the ring, never bodies or key names. `reiki top` shows those counts for every peer and the full recent list only for the local instance when its admin token is available.

### Fixed

- A 402 whose body says the request asked for more output tokens than affordable (for example messages containing `fewer max_tokens` or `can only afford`) is passed through to the client instead of cooling the key, so the client can retry with a smaller request. Other 402 responses still cool the key as before.

## 0.10.4

### Added

- `reiki top` frame: a header panel with version, UTC clock, live or paused state and key hints; a provider tab row that follows `f`; a titled Peers panel; a Cooling panel shown only while something cools; a footer of toggle chips (detail, gauge, chart, filter, paused).
- `reiki top` chart row at 28 rows or more: a line chart of the selected peer's load, disk, sessions or requests (or the fleet's when none is selected), and a key-count chart by state. `m` cycles the chart metric.
- `reiki top`: `?` lists every key; any key closes the list.
- Docs: an overview, a configuration reference for every key, a command-line reference, and pages for failover and traffic shaping. The docs site gets a menu, and diagrams are drawn with mermaid.

### Changed

- Reiki is described as a security proxy for agent and LLM traffic: isolates keys, filters what leaves, keeps access reliable. The README, crate description and `--help` use that line.
- The interactive `reiki top` screen is drawn with the `fw-tui` widgets. Short terminals drop the chart row, then the cooling panel, then the tabs, and fold the header before the matrix is cut. `--once` output is unchanged.

### Fixed

- The sample systemd unit in `docs/service.md` made the key-health file read-only under `ProtectSystem=strict`, so health silently did not persist. It now adds `ReadWritePaths=/etc/reiki`.

## 0.10.3

### Added

- `reiki top` shows a fleet-scaled gauge of the last hour's requests per provider and a last-hour trend in the detail card, fed by `requests_1h`, `limited_1h` and `request_buckets` in the peer document (off with `[peers] expose_usage = false`).
- A quota 429 that carries `Retry-After` now cools the key for that long, and the failure logs the header and a bounded body snippet. A `quota_resets` schedule still takes precedence; with neither, the 5 hour window applies.

- The peer document carries `reiki_version`, shown by `reiki top` as a `Ver` column and after each peer's name in its card; `?` for an older peer.
- The peer document carries `tokens_1h` and `token_buckets` per upstream: input plus output tokens the provider reported, counted in the same hour window as the attempts, under `[peers] expose_usage`.
- `/metrics` gains `reiki_upstream_requests_1h`, `reiki_upstream_limited_1h` and `reiki_upstream_tokens_1h` per upstream, sampled from the same counters as the peer document. Absent with `expose_usage = false`.

### Changed

- Token counts are read from response bodies only when `[peers]` or the `metrics` plugin is configured, so an install with neither pays no per-response cost.
- `reiki top` opens a peer's detail pane for any peer you move to; Enter closes it.

### Fixed

- `reiki top` counts cooldowns down between snapshots instead of freezing them at the published value.
- A healthy key with a model cooling is drawn as a yellow `◐+N`, and the detail card says how many models are cooling and the last non-2xx status.

## 0.10.2

### Added

- Clean stop on SIGTERM and SIGINT, as a Linux service should. Listeners (client, admin, peers, tunnel) stop accepting at once, idle keep-alive connections close, requests in flight get `[service] shutdown_timeout_secs` (default 25), then the request log and state file are written and Reiki exits 0. A second signal exits at once. See [service](docs/service.md).
- systemd integration with no new dependency: `Type=notify` readiness (`READY=1`, `STOPPING=1`) and `WatchdogSec=` pings that stop when the async runtime is wedged. A warning at start when the open-file limit is below 4096.
- `[server]` sets the connection caps of the client listener (`max_connections`, default 1024; `max_connections_per_addr`, default 256) and the admin listener (`admin_max_connections`, default 64; `admin_max_connections_per_addr`, default 16). Loopback is exempt from the per-address cap. `reiki check` prints them, and a changed value on reload asks for a restart.
- `reiki top`: `f` cycles a provider filter, `--provider NAME` starts (or, with `--once`, prints) filtered to one provider, and the fleet line shows `ALERT <provider> all cooling, soonest <time>` when every peer that has a provider has it cooling or disabled.
- Plugins get an `on_shutdown` hook; the request log writes out its queue there.
- `reiki check` says when `[hubs]`, `[hub]` or `[accounts]` are configured but nothing serves them yet.

### Changed

- The client and admin listeners now cap connections per remote address (256 and 16 by default, loopback exempt). A busy agent host that opens more than 256 connections from one address needs a higher `max_connections_per_addr`.
- The key-health state file is written by a background thread, not inline on the request that records a failure. Writes that arrive meanwhile coalesce, and a stop flushes the last one.
- A relayed error body over 64 KiB is cut with `x-reiki-truncated: 1` instead of silently.
- In `reiki top`, a peer that went stale shows `Absent` for providers it never published instead of `?`.

### Fixed

- SIGTERM, which is what systemd sends, no longer kills Reiki on the spot with no drain and no flush; only Ctrl-C stopped cleanly before.
- A listener closes when told to stop, so connections during the drain are refused instead of left waiting, and idle keep-alive connections no longer hold the drain for its whole length.
- An upstream error body is scrubbed of the key before it is cut to 64 KiB, so a key echoed across the cut no longer leaks its head.
- Documentation: the forwarder environment line, the plugin-timeout summary and the listener caps now match the code.

## 0.10.1

### Added

- `reiki top` opens on a peers-by-providers matrix: one coloured cell per peer and provider (a provider a peer does not use is a dim dot, not a failure), host load with a sparkline, a disk bar and sessions, problem peers first, and a detail pane for the selected peer. `v` shows the full per-peer text view, `--full` starts in it, and `--once` prints the matrix as plain text. It scrolls with the selection, falls back to one line per peer on a narrow terminal, and pause freezes the screen with its age.
- The peers health document reports `cooling_combinations` per upstream, the number of route and model combinations cooling, as a count only, so model-level throttling is visible in `reiki peers` and `reiki top`.
- `make install` builds with `reiki top` and installs `reiki` into `~/.local/bin`, or `/usr/local/bin` as root (`PREFIX=` and `DESTDIR=` override).

### Changed

- `reiki top` and `reiki peers` show control characters and invisible format characters (bidi overrides, zero-width marks) in peer-supplied names as `?`, and measure names in terminal columns.

## 0.10.0

### Added

- Key health per route and model. A 402 or a plain rate-limit 429 cools only that route and model on the key, so a free-tier account's paid-model 402 no longer locks out its free models; other failures still cool the key, and three cooling models escalate to it (`scope_escalate_after`, default 3). `/status` and the metrics report the scope.
- Outbound shaping. An `[upstream.NAME.limits]` table (`rpm`, `tpm`, `concurrent`, `daily_requests`, `daily_tokens`, `min_rpm`, `max_wait_secs`, or a `preset`) gives each key a budget with provider-side buckets, a learned rate cap and persisted state. A key out of budget is skipped; if none is usable the client gets a 429 `reiki_shaped` with `Retry-After`. Token use is corrected from the response `usage`.
- `cooldown_class` (`rate_limited`, `quota_exhausted`, `transient`) in `/status`, `reiki keys --json` and the labelled peers view.
- `reiki top`, a live terminal view of the configured peers (included in release binaries; from source, cargo feature `top`).
- An optional `[hubs]`, `[hub]` and `[accounts]` config behind the `hub` cargo feature. It is parsed and validated only. See [hub](docs/hub.md).
- `[peers] max_connections_per_addr` (default 4).

### Changed

- `reiki_cooldowns_total` gains a `scope` label (`key` or `model`).
- A 402 or plain 429 now cools a route and model, not the whole key.
- Placeholder ids are stable across a config reload (one salt per process); a restart still changes them.
- The peers listener closes the connection after a 401 or 429, and failed loopback logins lock out per address. Tunnelled peers share that address.
- A response body that is not an event stream has a 300 s default total timeout (`response_timeout_secs` overrides it), and compressed input is capped at twice the decoded cap, 1 MiB minimum.
- A forwarder socket in a group- or other-writable directory without the sticky bit needs `expect_uid`, and upstream keys under 8 bytes are refused.

### Fixed

- A response with `Content-Encoding: identity` is scrubbed for the upstream key.
- The peers health view no longer lists per-model cooldowns, and `reiki peers` shows control characters in peer names as `?`.
- `Retry-After` on an exhausted chain counts only the tried upstream's keys; long model names are hashed into the scope; many paths on one model escalate to a key cooldown.
- The `placeholders` plugin leaves Anthropic `thinking` and `redacted_thinking` blocks and base64 `image` and `document` blocks byte-identical, so a second turn no longer fails signature validation.
- Placeholder terms match inside JSON-escaped strings, and a fragment held at the end of an Anthropic content block is flushed inside that block.
- The body rewrite compares `model`, its key and `drop_params` keys after decoding JSON escapes.
- Saved key health is not restored onto a key re-pointed at another upstream.

## 0.9.1

### Added

- Credential masks cover about 40 more vendor formats (Slack cookie and app tokens, Anthropic, Google, GitLab, Grafana, Sentry, Cloudflare and others; age keys). The list is `PREFIXES` in `reiki-redact`.

### Changed

- `reiki_requests_total` counts a fallback request once, under the upstream that served it, instead of also counting the primary's refusal.
- A 403 whose body does not name an invalid, expired or revoked credential skips the key for that request instead of disabling it permanently. A 401, or a 403 naming a bad credential, still disables it.

### Fixed

- A placeholder fragment held back when a stream ends is flushed instead of lost, and a placeholder split right after its first `[` is rejoined.
- Placeholder ids are 64-bit, and an id is never shared by two different values in a request.
- PHI and finance table scanning continues past a ragged row instead of stopping at it; a blank line still ends the table.
- The request log masks credential values in form-encoded and other `k=v&k=v` bodies, in requests and responses.
- The PHI, finance and controlled detectors find an identifier or banner right after a JSON-escaped `\n`, `\t` or `\r`; a multi-line prompt used to hide an SSN or card number from them.
- A detector in block mode fails closed if it panics, instead of forwarding the unscanned body.
- Overlapping masked terms, or a term overlapping a credential, are masked as one span, so no tail reaches the upstream.
- A response object declared a tool call by `"type": "tool_use"` (Anthropic) keeps credentials masked in its arguments whatever the fields are named.
- The log file keeps taking lines when a rotation fails; the failure is reported once.
- An intercepted tunnel closes after an hour like a passthrough one, and `allow_loopback` opens loopback only, not link-local or cloud-metadata addresses.
- The request log masks headers named like credentials (`api-key`, `x-auth-token`, `private-token`).
- Each proxied request reports one `RequestDone` event with the status the client received. This ends the double count on a fallback and the false 499 on 400, 408, 413 and plugin-block responses.
- State records a SHA-256 of each key, so rotation is detected without relying on mtime; state is written through a unique temp file and a directory sync. Older state without a digest still uses the modified time.
- A first transient 5xx no longer spends two attempts on one key, and the last upstream refusal is returned when no key serves the request.
- A later failure no longer shortens a longer cooldown on the same key.
- A response plugin gets its final chunk call when the upstream body errors midway.
- `reiki keys --check` reads each key file through one handle, and the key table aligns columns with non-ASCII names.
- A forward upstream with `send_keep_alive = true` is refused when the config loads, because its connection always closes.
- Forward upstreams apply `response_idle_timeout_secs` and `response_timeout_secs` to buffered and streamed bodies; a timed-out command releases its concurrency slot.
- A passthrough upstream receives the client's `key=` query parameter instead of having it stripped.
- A reload applies the `[log]` level and lists `[log]` changes that need a restart. A reload with no change no longer reports a spurious `tunnel.listen` restart, and the interception CA files are watched.
- `auth_header` and `auth_format` are checked when the config loads instead of turning into 401s that disable every key.
- Config syntax errors report line and column without the offending source line, in `/reload`, `/status` and the logs.
- `POST /reload` rejects a bare, empty or repeated `dry_run` with 400.
- A full login-throttle table evicts the weakest entry instead of ignoring new addresses, and an IPv4 peer of a dual-stack listener is counted as its IPv4 address.
- Admin and client auth accept the same spacing, and the percent decoder takes hex digits only.
- The log file is set to 0640, refuses a symlink, and counts failed writes.
- Startup config warnings reach the log, and SIGHUP reload and key re-enable no longer block the async runtime.
- The connection-limit warning is rate-limited, the file watcher detects a same-size swap, and listener collision checks compare literal addresses without a DNS lookup.
- `metrics` sees responses when it is the only plugin, and keeps `content-length`.
- `metrics` recognises `text/event-stream` regardless of case or parameters, parses event-stream usage in linear time with a bounded line buffer, saturates totals instead of overflowing, and attributes usage to the upstream that served the response.
- A changed `[plugins.metrics]` table keeps the running counters and reports a restart is needed.
- `RequestCtx::report_detail` filters and cuts a finding detail before allocating it.
- `PluginError::Reject`'s message is documented as public, because it is returned to the client and logged.
- Request ids are 32 hex digits.
- Plugin admin paths are validated and the reserved set comes from the real admin routes, so a plugin cannot shadow `/reload`, `/keys/enable` or a `/v1/...` form.
- The build script rejects a missing, empty or duplicate plugin catalog, and warns about orders that put `requestlog` before any masking plugin or detectors after `placeholders`.
- Upstream HTTP clients are built fallibly; a failure stops startup instead of silently using a client that follows redirects and decompresses responses.
- An error body that fails mid-read no longer looks like a clean end: the key is classified from the status and `Retry-After` only, and the incomplete read is logged at warn without the body.
- A poisoned rate limiter lock no longer disables a stream's limit.
- A configured intercept CA is validated at startup and by `reiki check`: the key must match, the CA must not be expired, carry a DNS name constraint and cover every intercepted host, and leaf certificates are clamped to the CA's validity. A cached leaf is reused only inside its own validity, so raising `leaf_ttl_hours` on a reload cannot revive an expired one.
- `reiki scan -p <plugin> -k <kind>` refuses a kind the selected plugin does not know instead of reporting a clean scan.
- `allow_loopback = false` also refuses 6to4 and Teredo addresses that embed a loopback, link-local or metadata address, and bracketed IPv6 tunnel allow rules (`[::1]:443`) match.

## 0.9.0

### Added

- `requestlog` option `queue_max_bytes` (default 67108864, 1 MiB to 4096 MiB) caps the total bytes of records queued for the writer thread. Before, the default queue could hold about 1 GiB of request bodies. A record that would pass the cap is dropped like a full queue: it is counted, the first drop logs one warning, and the request is never delayed.
- An optional `[peers]` table for health sharing between instances, behind a new `peers` cargo feature that is in the default set. It is off unless `enabled = true`: without the table, or with it disabled, no listener is opened, no peer is contacted, and `/status` and `/v1/openapi.json` are unchanged (golden and socket-table tests check this). A binary built without the feature refuses a `[peers]` table. See [peers](docs/provider-health.md).
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

### Changed

- The `placeholders` plugin masks configured terms in one Aho-Corasick pass over the folded text instead of one scan per term, so the cost no longer grows with the number of terms. Matching, case folding, overlap rules and placeholders are unchanged.

### Fixed

- The detection plugins and `placeholders` no longer treat `Content-Encoding: identity` as an encoded body. `identity` is a legal no-op, so it is now ignored (case-insensitive, trimmed, comma separated and repeatable) while any real or unknown coding still fails closed. Before, a request with `identity` was rejected with 415 in block mode and skipped in report mode, and `placeholders` refused it or did not restore the response.
- The detection plugins now fail closed on a request body that is not valid UTF-8 instead of scanning a lossy replacement-character copy, which could hide an identifier by breaking a digit run. `block` mode rejects it with 415 and a message saying so; `report` mode logs that it was not checked. `placeholders` already refused such a body.
- Detection results no longer count two overlapping extra hits twice. An accepted extra now covers its bytes for later extras, including when the hits have different kinds.
- The `phi` table detector reads a header only on the first line of a table and, when the header maps to the `column` kind, only when at least half of its non-empty cells are known column names. A CSV data row such as `alice,address` is no longer read as a header, so block mode no longer rejects ordinary tabular data.
- The `controlled` plugin reports `CUI`, `NOFORN` and `ORCON` only in banner or portion-marking shape: alone on a line, at the start of a `//` caveat chain, or inside a portion marking at the start of a paragraph. A mention in prose, in quotes or in a JSON string is no longer reported, so block mode no longer rejects ordinary requests. `docs/controlled.md` states exactly what matches.

### Security

- `requestlog` masks more credential query parameters in the logged path. The name is percent-decoded and normalised (lowercase, non-alphanumerics dropped) before matching, so `api-key`, `access-token`, `auth_token`, `api_secret`, `client_secret`, `signature`, `passwd` and `bearer` are masked, including repeated parameters and percent-encoded names such as `api%2Dkey`. Ordinary names such as `monkey`, `keyboard` and `page_token_count` are not.
- `requestlog` finds a credential anywhere in a base64-decoded value, not only at its start, so a base64 `user:sk-…`, a base64 JSON object carrying a token, or a key after a text prefix is masked whole.

## 0.8.3

### Fixed

- The x86-64 Linux release binaries 0.8.0 to 0.8.2 used AVX instructions in the TLS library's CPU detection and crashed with an illegal instruction on CPUs without them (older x86-64 CPUs without AVX were affected). The release now builds the C code for baseline x86-64 and the workflow runs `ca-init` on an emulated Nehalem CPU before publishing.

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

- The `gov` plugin is now `controlled` (crate `reiki-controlled`, feature `plugin-controlled`, docs `controlled.md`), since it looks for controlled-information markings and identifiers, not government data in general. `[plugins.gov]`, `reiki scan --plugin gov` and the `plugin-gov` feature worked then but were removed in 0.11.0. Findings now name the plugin `controlled`, so a dashboard or alert that matches `plugin="gov"` needs the new name.

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

- A `gov` plugin that blocks or reports CUI, classification and dissemination markings, plus label-bound EIN, ITIN, CAGE, UEI, EDIPI and ECCN values, in outbound request bodies. See [docs/controlled.md](docs/controlled.md).
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
