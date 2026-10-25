# Changelog

## Unreleased

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
