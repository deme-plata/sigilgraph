#!/usr/bin/env bash
# Build sigil-wallet-phrase for the browser → gui/wasm/sigil_wallet_phrase.wasm (served as
# sigilgraph.org/wasm/sigil_wallet_phrase.wasm, loaded by gui/sigil-wallet-phrase.mjs).
#
# A plain C ABI module: no wasm-bindgen, no generated glue, nothing to version-match. The
# crate-type is applied for THIS build only (`cargo rustc --crate-type cdylib`), the same rule
# and reason as scripts/build-shield-wasm.sh: cdylib in the manifest would drop the rlib's
# filename hash and invalidate every downstream native build.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
WASM=$TARGET_DIR/wasm32-unknown-unknown/release/sigil_wallet_phrase.wasm
FLUXC=${FLUXC:-$HOME/.flux/bin/fluxc}

RUSTC_WRAPPER="$FLUXC" FLUXC_WRAPPING=1 REAL_RUSTC=rustc \
  cargo rustc --locked --crate-type cdylib --target wasm32-unknown-unknown -p sigil-wallet-phrase --release

[ -f "$WASM" ] || { echo "FATAL: expected $WASM, not produced" >&2; exit 1; }
mkdir -p gui/wasm
cp "$WASM" gui/wasm/sigil_wallet_phrase.wasm
echo "==> gui/wasm/sigil_wallet_phrase.wasm $(stat -c %s gui/wasm/sigil_wallet_phrase.wasm) bytes, sha256 $(sha256sum gui/wasm/sigil_wallet_phrase.wasm | cut -c1-16)…"
