# no-grant reference plugin

A minimal Phase 1 Wasm plugin that requests no gated capabilities. It imports
the default-allowed interfaces and passes every request.

## Build

Requires `cargo-component` and a Wasm component target. From this directory:

```sh
cargo component build --release --target wasm32-wasip1
wasm-tools component embed ../../crates/reiki-wasm/wit \
  target/wasm32-wasip1/release/reiki_example_no_grant.wasm \
  -w plugin -o no-grant.component.wasm
wasm-tools component add-metadata --name reiki-manifest \
  --content-type application/toml \
  --file reiki-manifest.toml \
  no-grant.component.wasm -o no-grant.wasm
```

The rebuild script `./rebuild.sh` runs the same steps when the tools are
installed and copies the result to `no-grant.wasm`.
