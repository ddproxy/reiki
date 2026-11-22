# WASI imports reference plugin

A negative reference component used by the load-time refusal test. It imports
`wasi:filesystem/types@0.2.12` and `wasi:sockets/tcp@0.2.12` and carries a
valid embedded manifest. No grant table in Phase 1 can authorise those
interfaces, so `WasmEngine::load_component_bytes` must refuse it before
instantiation.

## Build

Authored as WAT; no Rust target or `cargo-component` is needed. With
`wasm-tools` on `PATH` and `perl` available:

```sh
./rebuild.sh
```

The script validates the component and injects `reiki-manifest.toml` as a
top-level custom section, which is where the host inspector reads it.
