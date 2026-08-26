#!/usr/bin/env bash
# Regenerate gui/wasm/ — the browser-callable shielded-send prover.
#
# This script is the ONLY supported way to build sigil-shield for the browser.
#
# Why a script instead of `crate-type = ["cdylib", "rlib"]` in Cargo.toml:
# declaring cdylib in the manifest makes cargo emit the lib WITHOUT its filename hash
# (a shared object needs a stable SONAME), so every sigil-shield unit collides on one
# unhashed target/<profile>/deps/libsigil_shield.rlib. Each build rewrites it, its mtime
# jumps, and cargo correctly invalidates the whole downstream graph. Measured 2026-08-26:
# a NO-OP `build -p sigil-node --profile release-fast` went from 1.1s to 63-107s and
# never converged. `cargo rustc --crate-type cdylib` applies the crate-type for THIS
# build only, so native builds keep their hashed filename and stay incremental.
# Guard: crates/sigil-shield/tests/no_cdylib_in_manifest.rs.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=$ROOT/gui/wasm
WASM=$ROOT/target/wasm32-unknown-unknown/release/sigil_shield.wasm
FLUXC=${FLUXC:-/home/storage/deepseek-codewhale/flux/target/debug/fluxc}

# wasm-bindgen's CLI and the wasm-bindgen crate linked into the module must be the SAME
# version or the glue-generation step hard-fails. Take the truth from Cargo.lock.
WANT=$(awk '/^name = "wasm-bindgen"$/{f=1;next} f&&/^version = /{gsub(/[",]/,"");print $3;exit}' Cargo.lock)
[ -n "$WANT" ] || { echo "FATAL: wasm-bindgen not in Cargo.lock" >&2; exit 1; }

if ! command -v wasm-bindgen >/dev/null 2>&1; then
  echo "FATAL: wasm-bindgen CLI not found on PATH." >&2
  echo "       Install the version pinned by Cargo.lock (a mismatch fails at glue-gen):" >&2
  echo "         cargo install wasm-bindgen-cli --version $WANT" >&2
  exit 1
fi
HAVE=$(wasm-bindgen --version | awk '{print $2}')
[ "$HAVE" = "$WANT" ] || {
  echo "FATAL: wasm-bindgen CLI is $HAVE but Cargo.lock pins $WANT." >&2
  echo "       cargo install wasm-bindgen-cli --version $WANT --force" >&2
  exit 1
}

echo "==> building sigil-shield as a wasm cdylib (crate-type overridden at build time)"
# RUSTC_WRAPPER=fluxc keeps this on the Flux content-hash cache (dogfooding); the
# --crate-type override is why this is `cargo rustc` and not `fluxc build`.
RUSTC_WRAPPER="$FLUXC" FLUXC_WRAPPING=1 REAL_RUSTC=rustc \
  cargo rustc --crate-type cdylib --target wasm32-unknown-unknown -p sigil-shield --release

[ -f "$WASM" ] || { echo "FATAL: expected $WASM, not produced" >&2; exit 1; }

echo "==> generating JS glue into $OUT"
mkdir -p "$OUT"
wasm-bindgen --target web --out-dir "$OUT" "$WASM"

echo "==> done"
ls -la "$OUT"
