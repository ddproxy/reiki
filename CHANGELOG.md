# Changelog

## Unreleased

### Added

- `failure_ring_size` (default 200, max 10000, `0` disables) replaces the fixed 64-entry ring; a smaller cap on reload drops the oldest rows.
- Failure rows carry `action` (`cooldown`, `rotate`, `pass-through`) and 200-byte snippets; `/v1/failures` and `reiki failures` accept `?stream=`, `reiki top` shows `action`, and `/v1/metrics` gains `reiki_failure_ring_entries` and `reiki_failure_ring_dropped_total`. Query `limit` stays capped at 64.
- `docs/provider-health.md` and generated `docs/provider-health.openapi.json` describe the peer `/v1/health` contract; `host` and `sessions` are optional extensions, off by default.
- Full-surface OpenAPI golden and `scripts/openapi-diff.py` report removed, renamed, retyped, newly required and constraint-changed fields.

### Changed

- Public-repo hygiene: documentation IPs and neutral upstream names in examples and tests; `make hygiene` rejects internal hostnames with allowlisted exceptions.
- Reiki probes state and log directories before binding; `[gateway] on_store_error = "closed"` exits on a state probe failure, `"open"` warns, and a log probe failure disables the log file for the run.
- Reader/wire documents ignore unknown fields and key states; mutations, config and state stay strict.
- Peer health OpenAPI allows unknown fields, requires `observed`, extends enums and marks `host`/`sessions` optional.
- Admin OpenAPI has shared `Error`/`ErrorEnvelope` schemas and content schemas for admin, stream-admin and plugin routes.
- List routes publish bounded arrays: `/v1/status`, `/v1/stream-console`, `/v1/streams` and `/v1/usage` cap at 256 with totals; `stream-console` has a 64-record audit tail.
- **Breaking:** `/v1/status` no longer publishes raw filesystem paths in `peers.tls_ca[].file`; entries now carry only `ca_not_after` and `ca_days_left`.
- **Breaking:** peer listener refusals (401/429/404/503) now return the shared JSON error envelope (`{"ok":false,"error":{"code","message"}}`) instead of empty or plain-text bodies.
- **Breaking:** timestamps on the wire are now RFC3339 UTC strings for instants named `*_at` and integer seconds only for durations named `*_seconds`. Renamed fields: failure rows `time` -> `time_at`, stream-console `last_used` -> `last_used_at` and `expiry` -> `expires_at`, stream-console and stream audit `time` -> `time_at`, status `peers.tls_ca[].ca_not_after` and `intercept.ca_not_after` -> `ca_not_after_at`, and stream and token `created_at` values stay named but are now RFC3339 instants alongside RFC3339 `expires_at`. Persisted state and audit log file formats are unchanged; only JSON responses changed.
- **Breaking:** one cooldown vocabulary is used on the wire: status key and scope `retry_after_seconds`, observed `retry_after_seconds`, and failure `cooling_seconds` are now `cooldown_seconds`; `soonest_cooldown_seconds` and the raw `retry_after` header string keep their names. Plugin-internal fields, metric names and log fields are unchanged.
- **Breaking:** every listener now uses the same refusal envelope (`{"ok":false,"error":{"code","message"}}`), including client listener and plugin `400` answers; success bodies no longer carry `ok:true`. Mutations return the changed resource or `{}`: `/v1/reload`, `/v1/keys/enable`, and stream update/delete/revoke routes dropped `ok`, while stream create and token-add return one `token` object (`id`, `value`, `expires_at`) instead of `token_id`/`token`/`updated`/`deleted`/`revoked` variants, and replays of credential-minting writes return `{}`.
- **Breaking:** `/v1/failures` is now an envelope (`failures`, `failures_total`, `failures_truncated`) instead of a bare array, and other list routes keep one named array plus `<name>_total`/`<name>_truncated`.
- **Breaking:** the `metrics` plugin's admin usage route moved from `/v1/usage` to `/v1/usage/streams`; the client listener's self-service `/v1/usage` is unchanged.
- **Breaking:** published peer health arrays are bounded at 64 upstreams, 256 keys per upstream, 256 observed entries, 4 host disks and 256 session streams. Each array has a matching `*_truncated` flag and an OpenAPI `maxItems`; readers must use field presence rather than assuming unbounded arrays.
- Update dashboards, scripts and fleet consumers for renamed wire fields, error envelopes and the admin usage route.

### Removed

- **Breaking:** removed `[stream_admin].default_limits`; it was validated but never applied. Delete the table from configs before upgrading.
- **Breaking:** removed the `hub` Cargo feature and its parse-only configuration.
- **Breaking:** removed `[hubs.NAME]`, `[hub]`, `[hub.edges.NAME]` and `[accounts.LABEL]` tables; configs using them are now refused as unknown fields.
- **Breaking:** `reiki check` no longer prints the unserved-hub-tables line.
- **Breaking:** documentation pages were consolidated; deleted pages and old links now point to one owner page per topic.

## 0.15.1

### Added

- `reiki top` sorts the Streams table by name, usage, chain, rate or expiry with `o`, `O` and `r`; missing values sort last and ties keep usage order.

### Fixed

- The state writer probes counters every 5 to 30 seconds while failed and reopens admission after a successful write; `reiki_gateway_store_ok` exposes the state.
- `reiki top` Streams selection and detail follow streams by name across refreshes and re-sorts.

## 0.15.0

### Added

- Dynamic stream runbook with token commands, TTL ceilings, assignable keys, offline audit verify and backup/restore.
- Install and operations pages for status, manual install, release verification, upgrade, rollback, failure messages, logs and health checks.
- Checked README quickstart, `make docs-check`, `tests/docs_examples.rs` and CI validation of TOML, CLI flags, links, anchors and the proof.
- `AGENTS.md` and `llms.txt` as build and docs indexes.
- Dev pre-releases tag `vX.Y.(Z+1)-dev.<count>.g<hash>`, dispatch the release workflow and keep the ten newest; stable tags must match `Cargo.toml`.
- `scripts/set-release-version.py` validates stable tags, rewrites dev tags locally and orders dev builds.
- Final-response plugin phase: `Plugin::needs_final_response` and `Plugin::on_final_response` with bounded buffering.
- `[admin] read_token_sha256` opens read routes and gets 403 on mutations; `reiki top` prefers `REIKI_ADMIN_READ_TOKEN`.
- `reiki::admin_audit` audit event with bounded `X-Reiki-Actor`.
- `reiki top` sorts the peer matrix by column (`o`, `O`, `r`) and gains a read-only Streams screen (`t`) backed by `GET /v1/stream-console`.

### Changed

- README and docs follow one template; the docs index lists every page.
- Upgrade notes cover new keys, fail-closed behavior, state paths and file modes; stream administration is cross-linked across docs.
- `REIKI_PLUGINS` rejects `toolpolicy` before `placeholders`; `toolpolicy` canonicalizes JSON arguments.
- Admin refusals use one envelope; stream errors use `error.code`, reload `422` carries `config_refused` and keys `404` drops the echoed key.
- Read-only callers get redacted failure notes, reload errors, state errors and audit reasons.
- Wasm manifests reject log and reject template keys over 256 bytes at load time.
- Documentation for features introduced in 0.14.0 is checked against the binary; behavior is unchanged and the checked-in version is 0.15.1.

### Fixed

- Wasm host input bounds cap guest strings, ids, arguments and keys; finding-kind interning is isolated per plugin.
- Config validation treats a wildcard client listener and a loopback admin or tunnel listener on the same port as a collision.
- A decoded, buffered success keeps its gateway estimate; gateway completions stay in the admission window.
- Failed audit appends refuse mutations; docs links, version strings and paths were corrected.
- Regression tests cover Wasm bounds, config validation and documentation checks.

### Security

- Docs spell out the dedicated stream-management token, mode `0600` files, fail-closed audits and the plugin trust boundary.
- Builtin credential patterns match decoded JSON values; escaped matches are restored.
- `toolpolicy` checks the final restored response body.

## 0.14.0

### Added

- Opt-in `[stream_admin]`: dedicated token, `/v1/streams` CRUD and token/revoke routes, strict JSON, `Idempotency-Key`, two-party confirmation, assignable keys, ceilings, hash-chained JSONL audit with `audit.head` and mode-0600 state.
- `reiki streams` list, show, create, update, revoke, token and audit commands; feature-gated metrics, OpenAPI paths, status fields and peer counts.
- `plugin-wasm` (off by default): D5 Wasm components with `on-request`, gated body/detail grants, manifest pinning, host JSON edits, traps, reload narrowing, `reiki plugin inspect` and tool allowlists.
- Strict `observed` array in `/v1/status` and peer health with `(upstream, account)` class, `Retry-After` and quota headers; `[upstream.NAME] account` defaults to `default`.
- `/v1/metrics` gains `reiki_passthrough_requests_total`, `reiki_observed_cooldown_seconds` and `reiki_upstream_unaccounted_1h`; `reiki top` shows `observed/{account}`.
- `[peers] allowed_urls`, `peers.tls_ca` expiry and rate-limit header parsers for generic, OpenAI and Anthropic bodies.
- `no-grant.wasm` and `body-edit.wasm` fixtures, conformance suite and `reiki plugin inspect` tests.

### Changed

- `plugin-wasm` pins wasmtime and wasmtime-wasi to 51.0.0; the loader refuses WASI filesystem, socket and HTTP imports.
- `[streams.NAME.limits]` adds `rpm`, `tpm`, `concurrent`, daily/monthly budgets, `period_zone`, allowlists and `weight`; `[gateway] on_store_error` (default `closed`) sets fail-closed counters.
- Client `GET /v1/usage` reports the caller's totals with `[gateway] usage_rpm`; `Event::Usage` emits final token counts; gateway refusals and store failures expose metrics.
- Gateway counters persist in `<config>.state`; a changed stream token resets them; peer health publishes `gateway` counts with `expose_usage`.
- `plugin-cost` (off by default): `[plugins.cost] prices` with exact-then-glob rules, micro-unit state by hour/day/month, per-stream budgets with `on_exceed`, `GET /v1/cost`, `reiki cost export`, Prometheus series and a `reiki top` spend line.
- Stream admission replaced `rate_limit_per_minute` with `[streams.NAME.limits]`; admission happens once before key selection and `/v1/status` streams carry `limits` and `usage`.
- `days_from_civil` moved to `reiki-plugin::timefmt`; `reiki_health::Health` requires `observed`; client `X-Reiki-*` headers are stripped; the plugin API adds budget refusals, cost fragments, `AdminCtx::query` and `PluginHost` state.

### Fixed

- The Wasm host accepts `wit-bindgen` imports; edit components can declare `request-body-read`; `memory.grow` traps are classified as memory limits; deadlines are rechecked after host calls.
- Peers accept `capacity_group`; self-listener URLs are refused; `reiki keys` accepts `observed` and `peers` status fragments.
- Observed error bodies are replayed; CONNECT waiting for a head is closed at drain; tunnel idle detection uses a monotonic clock; cancelled plugin sessions fail closed.
- Docs navigation was deduplicated, `service.md` links removed and admin wording matched to `/v1`.

## 0.13.0

### Added

- `[upstream.NAME] capacity_group` on `/v1/health` and in `reiki top`; health types in `reiki-health`.
- `[plugins.requestlog.export]` summary export to rotating JSONL or RFC 5424 syslog with a bounded queue and drop metric.
- `toolpolicy` checks tool calls in OpenAI and Anthropic responses with `report` or `block`, a JSONL journal and admin metrics.
- `quota_reset_at` on labelled key views; `[tunnel] close_idle_on_stop` (default `true`); fuzz targets with a `fuzzing` feature.

### Changed

- Vendor phrases must come from `[upstream.NAME] quota_phrases`; `limits.preset` names are data-table examples.
- `requestlog` records summaries by default with `body = "summary"` and `capture_bodies = false`; capture requires opt-in and masks bodies.
- Security docs spell out token scope and masking limits; a test refuses another stream's token on an intercepted host.
- Workspace, fuzz and helper crates use edition 2024; plugin hooks run on the blocking pool and panics fail closed with 503.

### Fixed

- Fixed error descriptions, Unicode placeholder matching, chunk-boundary scrubbing, eight-layer percent-decoding, `/status` JSON fragments, 404 on all unknown admin methods and peer publisher locking.

### Removed

- Old key state formats, config aliases, unversioned admin routes, compatibility fields, older peer and top formats and `docs/service.md`.

## 0.12.0

### Added

- State recovery and body-timeout regression tests; `reiki top` `ALL` row and per-upstream chart with `s` overlay or stack.

### Changed

- `[admin].listen` must be a literal loopback IP and port; `strict_key_permissions` (default `true`) rejects readable keys.
- `reiki top` `u` cycles chart series; stale peers are excluded and the X axis uses the sample interval.

### Fixed

- Auth throttle reserves attempts under one lock; plugin `new_state` and `on_shutdown` panics fail closed per `on_error`.
- Placeholders handle long JSON strings; missing state fails closed; probes use `create_new` and `O_NOFOLLOW`; the log sink refuses symlinks; key permissions use the read handle.
- CONNECT keeps its authorised host; unknown codings are refused with 502 when a key is in play; the scrubber reserves raw and split forms; `/status` gains `log.write_errors`; over 4096 JSON members are refused with 400.
- Watch bounds and leaf-cache resets; forwarder `max_stream_bytes` and `stream_timeout_secs`; strict `?key=`; credential-named header refusal; write-bit checks; bounded SSE data; log file `O_NONBLOCK` clear; longest-first scrubber forms.

## 0.11.0

### Fixed

- A 402 for unaffordable tokens is `request_error` with `cooled: false`; the scrubber covers escapes, percent-encoding, base64 and split keys and withholds snippets with key material.

### Added

- Per-upstream phrase lists, a startup state-directory probe and panics logged through `tracing`.

### Removed

- `plugin-gov`, its config table, scan alias and build flag; use `controlled`.

## 0.10.6

### Changed

- Columnar PHI and finance scanning lives once in `reiki-detect`.

### Fixed

- A `free-models-per-day` 429 cools the whole key as quota exhaustion.

## 0.10.5

### Added

- A bounded 64-entry key-failure ring; `GET /failures` and `GET /v1/failures` with `limit` and `key` filters; `reiki failures`.
- Peer health publishes failure counts by class; `reiki top` shows them for peers.

### Fixed

- A 402 for unaffordable output tokens passes through without cooling the key.

## 0.10.4

### Added

- `reiki top` frame with tabs, panels, toggle chips and a chart row with `m` cycling metrics; overview, configuration, CLI, failover and shaping docs.

### Changed

- Reiki is described as a security proxy for agent and LLM traffic; `reiki top` uses `fw-tui` with progressive layout folding.

### Fixed

- The systemd unit grants `ReadWritePaths=/etc/reiki`.

## 0.10.3

### Added

- `reiki top` fleet request gauge and trend; quota 429 with `Retry-After` cools the key; peer documents carry `reiki_version` and token counts.
- `/metrics` gains `reiki_upstream_requests_1h`, `reiki_upstream_limited_1h` and `reiki_upstream_tokens_1h`.

### Changed

- Token counts are read only with `[peers]` or the metrics plugin; `reiki top` opens detail on move.

### Fixed

- `reiki top` counts cooldowns between snapshots and shows model-cooling state.

## 0.10.2

### Added

- Clean SIGTERM and SIGINT stop with drain via `[service] shutdown_timeout_secs` (default 25), flush and exit 0; a second signal exits at once.
- systemd readiness and watchdog pings; `[server]` connection caps (1024/256 and 64/16, loopback exempt); `reiki top` provider filter; plugin `on_shutdown`.

### Changed

- Key-health writes move to a background thread; error bodies over 64 KiB are cut with `x-reiki-truncated: 1`; stale peers show `Absent`.

### Fixed

- SIGTERM drains; listeners close on stop; error bodies are scrubbed before truncation; docs match the code.

## 0.10.1

### Added

- `reiki top` peers-by-providers matrix with `v`, `--full` and `--once`; `cooling_combinations` in peer health; `make install` installs into `PREFIX/bin`.

### Changed

- Control and invisible format characters in peer names show as `?` and names are measured in terminal columns.

## 0.10.0

### Added

- Per-route and per-model key health with `scope_escalate_after` (default 3); outbound shaping via `[upstream.NAME.limits]` with a 429 `reiki_shaped`.
- `cooldown_class`, `reiki top`, optional `[hubs]` behind the `hub` feature and `[peers] max_connections_per_addr` (default 4).

### Changed

- A 402 or plain 429 cools a route and model; placeholder ids stay stable across reload; the peers listener closes after 401/429; non-SSE responses have a 300 s timeout; forward sockets need `expect_uid` and keys under 8 bytes are refused.

### Fixed

- `Content-Encoding: identity` responses are scrubbed; `Retry-After` counts only tried keys; Anthropic thinking and image blocks stay byte-identical; saved health is not restored onto a re-pointed key.

## 0.9.1

### Added

- About 40 more credential masks in `reiki-redact`.

### Changed

- `reiki_requests_total` counts a fallback once under the serving upstream; a 403 without a bad-credential marker skips the key for that request.

### Fixed

- Placeholder fragments, ragged tables, form-body masking, escaped newlines, fail-closed panics, overlapping spans, tool arguments and log rotation.
- Tunnel hour cap, loopback-only `allow_loopback`, credential-named headers and one `RequestDone` per request.
- Key digests in state, attempt and cooldown fixes, forwarder timeouts, `key=` on passthrough, reload CA watching, strict `dry_run`, login-throttle eviction, log permissions, plugin metrics, request ids, plugin validation, fallible clients, intercept CA checks and `reiki scan` kinds.

## 0.9.0

### Added

- `[plugins.requestlog] queue_max_bytes` (default 67108864) caps queued bytes.
- `[peers]` behind the default `peers` feature, off unless `enabled = true`, with validation and `reiki check` output.
- Peer listener publishes `GET /v1/health` and `GET /v1/health/stream` with key counts, host stats, sessions, named tokens, TLS and `cooldown_bucket_secs`.
- `reiki peers` and `reiki peers --watch` read and follow health with per-peer TLS trust, timeouts and freshness checks; the feature adds `rustix`.

### Changed

- `placeholders` masks terms in one Aho-Corasick pass.

### Fixed

- `identity` is a no-op; invalid UTF-8 bodies fail closed; overlapping hits are counted once; detection shapes are narrowed.

### Security

- `requestlog` masks normalized credential query parameters and base64-decoded credentials.

## 0.8.3

### Added

- `[plugins.requestlog] max_capture_bytes` (default 1048576); `reiki_plugin_rejections_total` and `Event::PluginRejected`; `hold_unauthorized = true`.

### Changed

- Release builds use baseline x86-64 C code and an emulated CPU check.

### Fixed

- AVX crash on older CPUs; log rotation retries; tool-call bodies keep formatting.

### Security

- More vendor prefixes, encoded keys and split secrets are masked; echoed keys are scrubbed in encoded and streamed forms.

## 0.8.2

### Added

- Thin LTO and stripped release symbols; `deny_private` covers more reserved and embedded addresses.

### Changed

- Masking plugins fail closed via `Plugin::fails_closed()`; `on_error = "pass_through"` restores the old behavior.

### Fixed

- Reload checks the CA for new hosts; IPv6 login counts are per /64; syslog resolution has a timeout.

## 0.8.1

### Added

- `POST /keys/enable?key=<name>`, `reiki keys --enable` and `[auth]` login settings.

### Security

- Bodies are decoded and capped at `max_decoded_bytes` (default 64 MiB); stream limits count every attempt; body timeouts stop stalled responses; ambiguous POST/PATCH retries stop; admin failures are logged and throttled; `hash-token` needs 16 characters; the CA key is checked on load and reload.

### Fixed

- Failed-login eviction drops the weakest address; placeholders recognize `functionCall` and `toolUse`.

## 0.8.0

### Added

- Finding `detail` labels with log and OCSF output; `streams = [...]` plugin scoping; CA reload; `requestlog` syslog output.

### Changed

- The `gov` plugin is `controlled`; findings name `controlled` and old forms were removed in 0.11.0.

## 0.7.8

### Fixed

- Keys echoed in successful responses are scrubbed, including split streamed chunks.

## 0.7.7

### Added

- `reiki keys` shows upstream, state, cooldown and last status, with `--check` and `--json`.
- Versioned `/v1/*` routes, `GET /v1/openapi.json`, `X-Reiki-Version`, token metrics and admin `GET /usage`.

## 0.7.6

### Fixed

- OCSF maps `PATCH` to HTTP Activity id 9 and findings carry `finding_info.created_time`.

## 0.7.5

### Added

- Live reload via SIGHUP and `POST /reload`, with previews, watch mode and restart labels.
- Finding metrics, `reiki scan`, request-log findings and OCSF 1.3 output.

## 0.7.4

### Added

- `gov` and `finance` detection plugins and the shared `reiki-detect` crate.

### Changed

- A bare CUSIP needs a label or column.

### Fixed

- Large tables and unclosed `INSERT` bodies scan in linear time.

## 0.7.3

### Added

- `phi` table scanning with a `column` kind; `pass_errors = true` relays error statuses without cooling the key.

## 0.7.2

### Added

- `phi` detection of PHI and NPPI identifiers; `[tunnel] deny_private`.

## 0.7.1

### Added

- `streaming = true` on forward upstreams; forwarders serve intercepted hosts; `confirm_quota = true` retries a quota 429.

### Changed

- Docs describe Reiki as an isolation proxy; term masking folds the body once.

### Security

- Placeholders stay masked in tool arguments; post-rotation failures do not disable the rotated key; decoded bodies cap at 512 MiB; logins are logged and throttled.

## 0.7.0

### Added

- `[upstream.X.forward]` socket and command forwarders with `expect_uid`, `pass_env`, `max_concurrent` (default 16) and config warnings; `[tunnel.intercept] require_auth` (default true).

### Security

- Intercepted CONNECT requires the stream token; one request disables at most one key; dot segments and encoded separators are refused; hop-by-hop and account headers are removed; echoed keys are redacted.
- Slow headers and bodies time out; tunnels have a deadline, per-peer cap and one-hour limit; the admin token differs from stream tokens; state is mode 0600; release builds are isolated.

## 0.6.0

### Added

- Plugin API events, admin paths and `uses_bodies`.

### Changed

- Metrics moved to the `metrics` plugin behind `[plugins.metrics]`; upstream requests use reqwest and faithful transports.

## 0.5.0

### Changed

- Intercepted requests use a faithful hyper client that keeps header case and order; encoded responses are decoded; reverse-proxy-only transport settings.

## 0.4.3

### Added

- Upstream `title_case_headers` and `send_keep_alive`, both off by default.

### Changed

- The upstream credential header is sent first.

## 0.4.2

### Added

- `[log]` rotating file, levels and filters; request ids in log lines; intercept refusals logged; request-log body shaping and SSE formats; `reiki ca-init --replace`.

### Changed

- SSE bodies default to parsed events; a 429 with `free usage limit` counts as quota exhaustion.

## 0.4.1

### Added

- `paths` on an intercepted host relays multiple prefixes.

## 0.4.0

### Added

- `[tunnel.intercept]` TLS termination and relay through the normal route; `reiki ca-init` name-constrained CA; `REIKI_PLUGINS` build order from `Cargo.toml`.

### Changed

- The tunnel config gains an optional `intercept` table; provider docs moved to `docs/providers.md`.

## 0.3.0

### Added

- `placeholders` masks and restores credentials and configured terms; upstream `fallback`, `passthrough`, `[tunnel]` CONNECT proxy and `reiki-redact`.
- Plugin path, key and `on_attempt` context; request-log attempts and masked bodies; per-upstream `http2` and `forward_accept_encoding`.

### Changed

- Response hooks run in reverse plugin order; plugin contexts take the path and key name.

## 0.2.0

### Added

- Build-time plugin API in `reiki-plugin`; `requestlog` writes rotating JSON lines and a live socket feed with masked credentials.

### Changed

- Plugin panics are caught; configured plugins drop `content-length`; the 8 MB body limit applies after plugins; SIGHUP reload validates plugins; `reiki check` lists configured plugins; `App::new` takes the plugin chain.

### Fixed

- Error bodies are valid JSON for every message.

## 0.1.2

### Added

- Optional admin listener with `/status` and `/metrics`; distinct listen addresses; per-stream rate limit; model rewrites, quota resets and key health in `<config>.state`.

### Fixed

- Case-insensitive Bearer, serialized state writes, no state replay on SIGHUP, poisoned locks, loopback-only plain HTTP, capped restored cooldowns and MSRV 1.99.

Earlier releases: see the GitHub releases page.
