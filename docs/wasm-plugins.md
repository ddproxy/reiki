# Wasm plugins (Phase 1)

Phase 1 loads runtime Wasm component plugins behind the `plugin-wasm` Cargo
feature. The feature is off by default; a binary built without it refuses
`[plugins.<name>]` tables that contain `wasm = ...` with a clear config error.

Runtime Wasm plugins run in the fixed D5 inner slot: after compiled-in request
plugins and before compiled-in response observers (`requestlog`, `metrics`,
`toolpolicy`). On request they run after placeholders; on response they are
masked before response observers. They are not compiled-in builtins and are not
selected by `REIKI_PLUGINS`.

## Operator guide

### Configure and pin

```toml
[plugins.example]
wasm = "/var/lib/reiki/plugins/example.wasm"
sha256 = "<64 lowercase hex>"
# optional:
# role = "security"
# on_error = "reject"          # or "pass_through"
# streams = ["agent"]
# grants = ["request-body:read", "finding:detail-vocab"]
# confirm = "I understand this grant"
# tool_allowlist = ["search", "lookup"]
# [plugins.example.limits]
# memory-mib = 16
# fuel-per-call = 10000000
# fuel-per-kib = 200000
# deadline-ms = 250
# wall-budget-ms = 1000
# max-body-kib = 1024
# [plugins.example.config]
# plugin-specific = true
```

- `wasm` and `sha256` are required. `sha256` pins the exact component bytes;
  a mismatch fails load. The embedded `reiki-manifest` custom section must also
  match the configured name and declared/requested capabilities.
- Unknown config keys are errors.
- `signature` and `files` are reserved and not supported in Phase 1.

### Grants and capabilities

Capabilities are derived from component imports (D1). Default-allowed Phase 1
capabilities are linked for every plugin: `request-meta`, `finding`, `log`,
`reject`, `clock:monotonic`, `random`, `request-state`.

Filesystem, network, and HTTP access are not Phase 1 capabilities. The host
pins `wasmtime` and `wasmtime-wasi` to 51.0.0. It refuses to load a component
that imports any `wasi:filesystem`, `wasi:sockets`, or `wasi:http` interface.
No `grants` entry can enable those interfaces. The tiny preview1 adapter used
by the reference fixtures does not implement filesystem or socket calls, so
those calls fail at runtime.

Gated Phase 1 capabilities must be granted in config and declared in the
manifest:

| Grant | Risk | Notes |
| --- | --- | --- |
| `request-body:read` | med | Read request body bytes. |
| `request-body:edit` | med | Return host-applied edit ops; host revalidates JSON. |
| `finding:detail-vocab` | low | Finding details must be in manifest vocab. |

A plugin requesting a gated capability not granted by config fails to load. A
config grant not requested by the component also fails to load. Default-allowed
capabilities must not be listed in `grants`.

### Limits

Effective limits are `min(manifest, operator override, host ceiling)`. Operator
overrides in `[plugins.<name>.limits]` may only lower manifest limits.

| Limit | Manifest default | Host ceiling |
| --- | ---: | ---: |
| `memory-mib` | 16 | 128 |
| `fuel-per-call` | 10,000,000 | 2,000,000,000 |
| `fuel-per-kib` | 200,000 | 20,000,000 |
| `deadline-ms` | 250 | 2,000 |
| `wall-budget-ms` | 1,000 | 10,000 |
| `max-body-kib` | 1,024 | 8,192 |

Fuel is charged per call with a body-size component (`fuel-per-call +
ceil(body_kib) * fuel-per-kib`). Exceeding fuel, memory, or deadline traps the
plugin; `role = "security"` or `on_error = "reject"` fails closed with 503.

`deadline-ms` bounds a request, including time spent inside host calls.
Load-time instantiation and `init` are a separate phase and use `wall-budget-ms`
instead; the per-request `init` call is part of the request and uses the
request deadline. Both budgets are checked by host calls and by the epoch
ticker, so neither init nor a request can run unbounded; both are also capped
by their host ceilings.

### Restart rules and reload narrowing

Reload can only narrow. Widening requires restart.

- Grant removal is live narrowing; adding a grant requires restart.
- Stream removal is live narrowing. Adding a stream is live for low-only grants;
  adding a stream to a plugin with med/high grants requires restart (D6).
- Limit lowering is live; raising a limit (or removing an override that would
  raise effective limit) requires restart.
- `pass_through -> reject` and adding `role = "security"` are live; the reverse
  requires restart.
- `tool_allowlist` tightening is live; removing it or adding allowed tools
  requires restart.
- New Wasm bytes/hash: live when no high-risk grant is present; high-risk grants
  require restart/confirm flow as configured.

### Request-side tool allowlist

`tool_allowlist = [...]` is a host-enforced Phase 1 request-side allowlist. For
recognized request formats, Reiki reuses `reiki-toolpolicy` request parsing to
extract declared tools. A declared tool not in the allowlist is rejected with
403. Unrecognized paths are unaffected; an unparseable recognized request fails
closed.

### Inspect

```sh
reiki plugin inspect ./plugin.wasm
reiki plugin inspect ./plugin.wasm --json
```

Inspect reports the embedded manifest, derived capabilities, default-allowed
capabilities, requested gated capabilities, manifest limits, and pin metadata.

## Plugin author guide

Phase 1 Wasm plugins target WIT world `reiki:plugin/plugin@1.0.0` from
`crates/reiki-wasm/wit/reiki.wit`.

Exports:

- `init: func(config: list<u8>) -> result<_, string>` - receives operator
  `[plugins.<name>.config]` as TOML bytes.
- `on-request: func(req: borrow<request>) -> result<tuple<verdict, option<request-edit>>, string>`.

Request edits are host-applied. The plugin returns `edit-op` values containing
JSON pointers and manifest literal IDs; the host validates pointers, resolves
literals from the embedded manifest, applies edits, and revalidates JSON.
Arbitrary rewrite is not supported in Phase 1.

The embedded manifest custom section is named `reiki-manifest` and contains
TOML. It declares plugin metadata, hooks, requested gated capabilities, literal
vocab, detail vocab, log/reject templates, and manifest limits. The manifest is
human-readable justification; the component import list is the capability
request (D1).

Reference examples:

- `examples/wasm-no-grant` - no gated capabilities.
- `examples/wasm-body-edit` - `request-body:edit` with a manifest literal edit.
- `examples/wasm-fs-probe` - a reference plugin that tries `open`, `truncate`,
  `rename`, and `hard_link` through WASI; the calls are refused.
- `examples/wasm-wasi-imports` - a negative component importing WASI
  filesystem and sockets; the host refuses it at load.

Each includes source, manifest, README, and `rebuild.sh`.

### Building without `cargo-component`

The reference examples do not use `cargo-component`. They are plain Rust crates
that call `wit-bindgen` at compile time, then use `wasm-tools` to wrap the core
module as a component. You need:

- the `wasm32-wasip1` Rust target (`rustup target add wasm32-wasip1`);
- `wasm-tools` on `PATH` (the scripts use the 1.263 command set);
- `perl`, used only to inject the manifest as a top-level custom section.

From an example directory, run `./rebuild.sh [OUTPUT]`. With no argument the
script writes `no-grant.wasm` or `body-edit.wasm` in that directory. The steps
are:

```sh
cargo build --release --target wasm32-wasip1
wasm-tools strip -a "$core" -o "$stripped"
wasm-tools component embed ../../crates/reiki-wasm/wit "$stripped" \
  -w plugin -o "$embedded"
wasm-tools parse "$adapter_wat" -o "$adapter"
wasm-tools component new "$embedded" \
  --adapt "wasi_snapshot_preview1=$adapter" -o "$component"
wasm-tools print "$component" -o "$component_wat"
# inject (@custom "reiki-manifest" "...") at the component top level
wasm-tools parse "$component_wat" -o "$out"
wasm-tools validate "$out"
wasm-tools component wit "$out"
```

The tiny inline WASI preview1 adapter supplies `environ_get`,
`environ_sizes_get`, `fd_write`, and `proc_exit`; it does not give the plugin
filesystem or socket access. Current `wasm-tools` has no
`wasm-tools component add-metadata` subcommand, so the manifest is injected
through the text format. The section must be at component top level: the host
inspector reads top-level custom sections only. `rebuild.sh` accepts
`REIKI_WASM_OUT`, `REIKI_WASM_WORK`, and `CARGO_TARGET_DIR` for reproducible
builds in a temporary directory.

### Phase 1 conformance coverage

The Phase 1 design's section 5 cases are exercised against the committed
components by `crates/reiki-wasm/tests/conformance.rs`:

| Case | Test or check |
| --- | --- |
| Small, hashed fixtures and rebuild comparison | `committed_fixtures_are_small_and_hashed`; `rebuilt_fixtures_match_committed_bytes` |
| Load with and without grants; import-derived capabilities | `no_grant_component_loads_and_runs_default_hooks`; `body_read_edit_and_vocab_findings_work_end_to_end`; `inspect_reports_import_derived_capabilities` |
| Pin mismatch and manifest mismatch | `pin_and_manifest_mismatches_fail_to_load` |
| Template skip/reject and sanitised reasons | `skip_and_reject_use_manifest_templates_and_sanitised_text` |
| Fuel exhaustion, memory limit, epoch deadline | `fuel_exhaustion_fails_closed`; `memory_limit_traps_and_fails_closed`; `deadline_reached_inside_host_call_is_enforced`; `init_is_bounded_by_the_wall_budget` |
| Trap handling per available hook | `trap_matrix_covers_init_and_request_by_role` (init load failure, and on-request trap with pass-through and fail-closed policies) |
| Body read/edit, vocab findings, invalid edit ops | `body_read_edit_and_vocab_findings_work_end_to_end`; `non_vocab_detail_is_dropped`; `invalid_edit_ops_are_rejected` |
| Request-side tool allowlist | `tool_allowlist_is_enforced_before_the_wasm_hook` |
| Reload that can only narrow | `reload_narrowing_uses_real_component_configs` |
| No WASI filesystem or network capability | `reference_plugin_cannot_open_truncate_rename_or_hard_link`; `wasi_filesystem_and_sockets_imports_are_refused_at_load`; `filesystem_and_network_cannot_be_granted` |
| Inspect output | `inspect_reports_import_derived_capabilities`; `tests/wasm_plugin_inspect.rs` checks `reiki plugin inspect` text and `--json` output for both fixtures |

Run the suite with:

```sh
cargo test -p reiki-wasm --test conformance
```

The rebuild comparison skips with a clear message when `wasm32-wasip1`,
`wasm-tools`, or `perl` is unavailable; when the toolchain is present it
rebuilds all four reference components into a temporary directory and compares
SHA-256 against `tests/fixtures/SHA256SUMS`.

Not covered by this Phase 1 pass: response-side hooks and the full design 3.3
hook matrix (Phase 1 exposes only `init` and `on-request`), the `max-body-kib`
limit (not exercised by the committed fixtures), and the reserved
`signature`/`files` fields (unsupported).
