# no-grant reference plugin

A minimal Phase 1 Wasm plugin that requests no gated capabilities. It imports
the default-allowed interfaces and passes every request.

## Build

No `cargo-component` is needed. Install the `wasm32-wasip1` Rust target and put
`wasm-tools` on `PATH`; `perl` is also needed to inject the manifest custom
section. From this directory:

```sh
./rebuild.sh
```

The script builds with `cargo build --release --target wasm32-wasip1`, strips
the core module, embeds `../../crates/reiki-wasm/wit`, wraps it with a tiny WASI
preview1 stub adapter using `wasm-tools component new`, injects
`reiki-manifest.toml` as a top-level custom section, and finishes with
`wasm-tools validate` and `wasm-tools component wit`. Current `wasm-tools` has
no `component add-metadata` command. Pass an output path to write elsewhere:
`./rebuild.sh /tmp/no-grant.wasm`. See [the plugin guide](../../docs/plugin-authoring.md#building-without-cargo-component)
for the full pipeline.
