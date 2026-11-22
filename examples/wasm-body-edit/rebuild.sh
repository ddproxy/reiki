#!/usr/bin/env bash
# Build the reference component without cargo-component: plain Rust -> core wasm,
# then wasm-tools embeds the WIT world, wraps the WASI p1 stub adapter, and adds
# the reiki-manifest custom section. No cargo-component is involved.
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
cd "$script_dir"

crate="reiki_example_body_edit"
plugin="body-edit"
manifest="$script_dir/reiki-manifest.toml"
wit="$script_dir/../../crates/reiki-wasm/wit"
out="${1:-${REIKI_WASM_OUT:-$script_dir/$plugin.wasm}}"

mkdir -p "$(dirname "$out")"
if [[ -n "${REIKI_WASM_WORK:-}" ]]; then
  work="$REIKI_WASM_WORK"
  mkdir -p "$work"
else
  work="$(mktemp -d "${TMPDIR:-/tmp}/reiki-$plugin.XXXXXX")"
  trap 'rm -rf "$work"' EXIT
fi
target="${CARGO_TARGET_DIR:-$work/target}"

CARGO_TARGET_DIR="$target" cargo build --release --target wasm32-wasip1
core="$target/wasm32-wasip1/release/$crate.wasm"

stripped="$work/core-stripped.wasm"
embedded="$work/embedded.wasm"
adapter_wat="$work/wasi-stub.wat"
adapter="$work/wasi-stub.wasm"
component="$work/component.wasm"
component_wat="$work/component.wat"

# Rust's wasm32-wasip1 core still imports a few preview1 functions for panic and
# stdio. The host links them inert; this tiny adapter satisfies component new.
cat >"$adapter_wat" <<'WAT'
(module
  (func (export "environ_get") (param i32 i32) (result i32) (i32.const 0))
  (func (export "environ_sizes_get") (param i32 i32) (result i32) (i32.const 0))
  (func (export "fd_write") (param i32 i32 i32 i32) (result i32) (i32.const 0))
  (func (export "proc_exit") (param i32))
)
WAT

wasm-tools strip -a "$core" -o "$stripped"
wasm-tools component embed "$wit" "$stripped" -w plugin -o "$embedded"
wasm-tools parse "$adapter_wat" -o "$adapter"
wasm-tools component new "$embedded" \
  --adapt "wasi_snapshot_preview1=$adapter" \
  -o "$component"

# wasm-tools 1.263 has no "component add-metadata" subcommand. Inject the same
# top-level custom section through the text format instead.
wasm-tools print "$component" -o "$component_wat"
REIKI_MANIFEST="$manifest" REIKI_SECTION="reiki-manifest" perl -0777 -i -pe '
  BEGIN {
    open my $fh, "<", $ENV{REIKI_MANIFEST} or die "cannot read manifest: $!";
    local $/;
    my $m = <$fh>;
    close $fh;
    $m =~ s/\\/\\\\/g;
    $m =~ s/"/\\"/g;
    $m =~ s/\r?\n/\\n/g;
    $ENV{REIKI_ESCAPED_MANIFEST} = $m;
  }
  s/\)\s*\z/  (\@custom "$ENV{REIKI_SECTION}" "$ENV{REIKI_ESCAPED_MANIFEST}")\n)\n/;
' "$component_wat"
wasm-tools parse "$component_wat" -o "$out"

wasm-tools validate "$out"
wasm-tools component wit "$out" >/dev/null
echo "wrote $out"
