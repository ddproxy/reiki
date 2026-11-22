#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
cargo component build --release --target wasm32-wasip1
wasm-tools component embed ../../crates/reiki-wasm/wit \
  target/wasm32-wasip1/release/reiki_example_body_edit.wasm \
  -w plugin -o body-edit.component.wasm
wasm-tools component add-metadata --name reiki-manifest \
  --content-type application/toml \
  --file reiki-manifest.toml \
  body-edit.component.wasm -o body-edit.wasm
echo "wrote $(pwd)/body-edit.wasm"
