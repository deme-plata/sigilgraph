#!/usr/bin/env bash
# Build the SIGIL Veil egress gate for the browser (SIGIL OS AI app).
#
# Same pattern as build-shield-wasm.sh: the crate is rlib-only in its manifest
# (a declared cdylib makes cargo drop the extra-filename hash and wrecks
# incremental builds workspace-wide), so the cdylib is applied HERE with
# `cargo rustc --crate-type cdylib` — the one documented raw-cargo exception,
# because fluxc cannot override a crate type.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${OUT:-$ROOT/gui/wasm}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
WANT=$(awk '/^name = "wasm-bindgen"$/{f=1;next} f&&/^version = /{gsub(/[",]/,"");print $3;exit}' "$ROOT/Cargo.lock")
command -v wasm-bindgen >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
HAVE=$(wasm-bindgen --version | awk '{print $2}')
[ "$HAVE" = "$WANT" ] || { echo "FATAL: wasm-bindgen CLI $HAVE != Cargo.lock $WANT" >&2; exit 1; }
echo "==> sigil-veil-wasm as a wasm32 cdylib"
( cd "$ROOT" && CARGO_TARGET_DIR="$TARGET_DIR" cargo rustc --crate-type cdylib --target wasm32-unknown-unknown -p sigil-veil-wasm --release )
WASM="$TARGET_DIR/wasm32-unknown-unknown/release/sigil_veil_wasm.wasm"
mkdir -p "$OUT"
wasm-bindgen --target web --out-dir "$OUT" "$WASM"
ls -la "$OUT"/sigil_veil_wasm*
