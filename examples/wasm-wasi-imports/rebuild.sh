#!/usr/bin/env bash
# Build the negative reference component used by the load-time refusal test.
# It imports a WASI filesystem interface and a WASI sockets interface. The host
# grants neither, so loading must fail before instantiation. No cargo-component
# (and no Rust toolchain) is needed: the component is authored as WAT.
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
cd "$script_dir"

plugin="wasi-imports"
manifest="$script_dir/reiki-manifest.toml"
out="${1:-${REIKI_WASM_OUT:-$script_dir/$plugin.wasm}}"

mkdir -p "$(dirname "$out")"
work="$(mktemp -d "${TMPDIR:-/tmp}/reiki-$plugin.XXXXXX")"
trap 'rm -rf "$work"' EXIT

cat >"$work/component.wat" <<'WAT'
(component
  (import "wasi:filesystem/types@0.2.12" (instance $filesystem))
  (import "wasi:sockets/tcp@0.2.12" (instance $tcp))
)
WAT

wasm-tools parse "$work/component.wat" -o "$work/component.wasm"
wasm-tools print "$work/component.wasm" -o "$work/component-print.wat"
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
' "$work/component-print.wat"
wasm-tools parse "$work/component-print.wat" -o "$out"

wasm-tools validate "$out"
wasm-tools component wit "$out" >/dev/null
echo "wrote $out"
