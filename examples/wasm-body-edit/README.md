# body-edit reference plugin

Phase 1 reference plugin that requests `request-body:edit` and returns a host-
applied edit op. The edit replaces `/model` with the manifest literal
`redacted-model`.

## Build

Requires `cargo-component` and a Wasm component target. From this directory:

```sh
cargo component build --release --target wasm32-wasip1
wasm-tools component embed ../../crates/reiki-wasm/wit \
  target/wasm32-wasip1/release/reiki_example_body_edit.wasm \
  -w plugin -o body-edit.component.wasm
wasm-tools component add-metadata --name reiki-manifest \
  --content-type application/toml \
  --file reiki-manifest.toml \
  body-edit.component.wasm -o body-edit.wasm
```

The rebuild script `./rebuild.sh` runs the same steps when the tools are
installed and copies the result to `body-edit.wasm`.
