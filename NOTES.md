# Security review notes

All seven findings were checked against the code before any change. None was wrong. Findings 1–5
were confirmed and fixed with one regression test each, in severity order. Findings 6–7 are
confirmed but Low, so they are outside the requested High/Medium fix scope and are recorded here
instead of being changed.

## Finding 1 — escaped builtin credentials are not masked (High, fixed)

`Masker::mask` scanned builtin patterns only over raw JSON text. A credential written with JSON
escapes (for example a `\u`-escaped prefix) therefore reached the model unmasked, while configured
terms already had a decoded-string path with `RawOffsets`. The fix scans decoded JSON string values
for builtin patterns too, maps decoded spans back to the whole escape, drops raw builtin hits that
fall inside a successfully decoded string, and keys every escaped match by its decoded value so
masking and restoration agree. Protected thinking/base64 spans still suppress matches. Regression
test: `builtin_credentials_are_masked_when_json_escaped` in
`crates/reiki-placeholders/src/mask.rs`.

## Finding 2 — toolpolicy checks raw, escaped argument strings (High, fixed)

`extract_calls` parsed `arguments` only to check validity and then kept the original string, and
the SSE accumulator passed its concatenated raw string to `check_call`. A host or tripwire spelled
with `\u002f`, `\u002e`, `\/`, or extra whitespace could evade `unapproved_host` and
`proc_environ`. `canonical_arguments` now parses and re-serializes the argument object before every
`check_call`, in both the buffered and SSE paths. Raw arguments are still used for body rewriting
and journal entries, so no response bytes or audit records changed shape. Regression tests cover
escaped hosts, escaped paths and mixed spellings in
`crates/reiki-toolpolicy/src/checks.rs` and `crates/reiki-toolpolicy/src/stream.rs`.

## Finding 3 — toolpolicy sees masked arguments before restoration (High, fixed)

The compiled order is `placeholders` before `toolpolicy`; response chunk hooks run in reverse, so
toolpolicy previously checked the masked body. Reordering the catalog would have broken the
`requestlog` masking rule, the wasm D5 slot marker and the metrics ordering assertion. The fix adds
a dedicated final-response phase: `Plugin::needs_final_response`/`on_final_response`, run in chain
order after all reverse chunk hooks. The host buffers the whole body only when a plugin opts in,
with an 8 MiB cap that fails closed. `toolpolicy` now leaves response bytes untouched in
`on_response_chunk`, so downstream plugins can rewrite them, and runs its existing buffered/SSE
pipeline on the complete post-plugin body in the final phase. A build-time rule rejects
`REIKI_PLUGINS` orders that put `toolpolicy` before `placeholders`. Regression test:
`toolpolicy_checks_the_restored_body_after_placeholders_run` in
`tests/toolpolicy_final_response.rs`.

Design decision left open: a general operator-facing policy for choosing which security checks see
raw versus restored response bodies. This change deliberately keeps the documented rule that terms
are restored in tool arguments while credential placeholders stay masked there by default, and
gives toolpolicy one late, fail-closed hook; it does not add a config switch or move other plugins'
response phases.

## Finding 4 — compressed successes zeroed gateway usage (High, fixed)

For decoded, buffered responses the proxy called `complete(0)` on every gateway permit. That
replaced the admission estimate with zero, so a compressed JSON success with an in-body `usage`
object showed zero tokens, contrary to `docs/shaping.md`. The permit is now simply dropped: the
concurrency slot is released and the estimate stays charged. The provider's `usage` object is still
ignored for compressed bodies. Regression test: the `/v1/usage` assertions added to
`a_compressed_response_is_not_watched_for_usage` in `tests/proxy.rs`.

## Finding 5 — gateway completion after a window rollover (Medium, fixed)

`Permit` stored only the estimate; `complete` read the current windows and could subtract the
estimate from a minute/day/month that never received it, zeroing a new window's tokens. The permit
now captures admission-time `minute_start`, `day_start` and `month_start` and only adjusts windows
that still match those starts. `complete_at` supplies an explicit clock for the minute-boundary
regression test. Regression test:
`a_completion_after_a_rollover_keeps_its_estimate_in_the_admitted_window` in `src/gateway.rs`.

## Finding 6 — config and health snapshots can disagree (Low, out of scope)

`src/proxy.rs` snapshots health and config in separate steps; a reload between them can pair one
key's identity with another's health. The safe fix is to take a single snapshot for both, but this
is a Low scheduling/identity issue with no High/Medium impact and is left unchanged here.

## Finding 7 — unbounded usage values (Low, out of scope)

`src/usage.rs` accepts `u64::MAX` totals and `src/gateway.rs` uses plain addition in a counter path.
The observed paths use saturating arithmetic at admission and completion, but the parser itself has
no upper bound. A bounded range or checked addition would be the fix; it is Low and deferred.

## Test data

No secret-shaped literals were added. Tokens, hashes and escaped prefixes in new tests are built at
runtime, following the existing fixture style.
