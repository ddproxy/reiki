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

Both include source, manifest, README, and `rebuild.sh`.
