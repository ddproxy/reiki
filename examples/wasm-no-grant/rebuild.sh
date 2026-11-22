#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
cargo component build --release --target wasm32-wasip1
wasm-tools component embed ../../crates/reiki-wasm/wit \
  target/wasm32-wasip1/release/reiki_example_no_grant.wasm \
  -w plugin -o no-grant.component.wasm
wasm-tools component add-metadata --name reiki-manifest \
  --content-type application/toml \
  --file reiki-manifest.toml \
  no-grant.component.wasm -o no-grant.wasm
echo "wrote $(pwd)/no-grant.wasm"
