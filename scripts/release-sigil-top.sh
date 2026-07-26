#!/usr/bin/env bash
# release-sigil-top.sh — codifies the FULL sigil-top release ceremony as one command.
#
#   scripts/release-sigil-top.sh 7.0.14 "one-line release note for the manifest"
#
# Steps (each fails LOUD; the sign-manifest step can never be skipped):
#   1. bump crates/sigil-top/Cargo.toml to the new version
#   2. build linux-x64 + windows-x64 (release, via fluxc — never raw cargo)
#   3. sign-artifact both (require-both SQIsign-L5 + Ed25519) + verify-proof
#   4. publish binaries + .proof + source tarball to dist-fluxapp/downloads
#   5. write sigil-top-latest.json manifest, then scripts/sign-manifest.sh it
#   6. verify the LIVE manifest+sig against the pinned RELEASE_SIGN_PUBKEY
#   7. git commit + annotated tag + push (branch + tag)
# The auto-updater rolls the new version to every node from the signed manifest.
set -euo pipefail

VER="${1:?usage: release-sigil-top.sh <version> [note]}"
NOTE="${2:-sigil-top v${VER}}"
PINNED_PUB="150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402"
REPO="/home/storage/deepseek-codewhale/sigil"
FLUXC="${FLUXC:-/home/storage/deepseek-codewhale/flux/target/debug/fluxc}"
DL="/home/orobit/q-narwhalknight/dist-fluxapp/downloads"
BASE="https://sigilgraph.fluxapp.xyz/downloads"
export PATH="/home/orobit/node/current/bin:$PATH"
cd "$REPO"

echo "▸ 1/7 bump version → $VER"
sed -i "s/^version = \"[0-9.]*\"/version = \"$VER\"/" crates/sigil-top/Cargo.toml
grep -q "^version = \"$VER\"" crates/sigil-top/Cargo.toml || { echo "✗ version bump failed"; exit 1; }

# 2026-07-26: the two builds now run IN PARALLEL. They used to be serial for
# ~100 s each (codegen-units=1 => one rustc, no intra-crate parallelism), so step 2
# dominated the ~4 min release. Naive `&` does NOT work: a cross build writes its
# host-side artifacts (proc macros, build scripts) into target/release/, so both
# cargos fight over target/release/.cargo-lock. MEASURED, no-op pair:
#   shared target/    19 s wall — 1x "Blocking waiting for file lock on artifact directory"
#   separate dirs     12 s wall — 0x artifact-directory lock (only the brief package-cache one)
# So the windows build gets its own CARGO_TARGET_DIR. One-time cost to establish it:
# 181 s and 888 MB (measured); warm after that.
# NOTE: two concurrent LTO links — give the wrapping systemd-run scope MemoryMax>=16G.
WIN_TARGET_DIR="${WIN_TARGET_DIR:-/home/storage/sigil-target-win}"
echo "▸ 2/7 build linux + windows (release, PARALLEL — windows in $WIN_TARGET_DIR)"
T_BUILD=$SECONDS
LLOG="/home/orobit/tmp/release-${VER}-build-linux.log"
WLOG="/home/orobit/tmp/release-${VER}-build-windows.log"
"$FLUXC" build --release -p sigil-top > "$LLOG" 2>&1 &
LPID=$!
CARGO_TARGET_DIR="$WIN_TARGET_DIR" "$FLUXC" build --release -p sigil-top --target x86_64-pc-windows-gnu > "$WLOG" 2>&1 &
WPID=$!
wait "$LPID" || { echo "✗ linux build failed:"; tail -25 "$LLOG"; exit 1; }
wait "$WPID" || { echo "✗ windows build failed:"; tail -25 "$WLOG"; exit 1; }
echo "  build wall clock: $((SECONDS-T_BUILD))s (serial baseline v7.1.9/v7.1.10: ~200s)"
LBIN="target/release/sigil-top"; WBIN="$WIN_TARGET_DIR/x86_64-pc-windows-gnu/release/sigil-top.exe"
[ -x "$LBIN" ] || { echo "✗ missing $LBIN"; exit 1; }
[ -s "$WBIN" ] || { echo "✗ missing $WBIN"; exit 1; }
"$LBIN" version 2>/dev/null | grep -q "v$VER" || { echo "✗ built binary is not v$VER"; exit 1; }

echo "▸ 3/7 stage + sign (require-both) + verify"
S="/home/orobit/tmp/sigil-v${VER}-release"; rm -rf "$S"; mkdir -p "$S"
cp "$LBIN" "$S/sigil-top-v${VER}-linux-x64"
cp "$WBIN" "$S/sigil-top-v${VER}-windows-x64.exe"
tar --sort=name --mtime='2026-01-01 00:00:00' --owner=0 --group=0 --numeric-owner \
    -czf "$S/sigil-top-v${VER}-src.tar.gz" -C crates sigil-top/src sigil-top/Cargo.toml
for t in "linux-x64" "windows-x64.exe"; do
  f="$S/sigil-top-v${VER}-${t}"
  "$FLUXC" sign-artifact "$f" --source "$S/sigil-top-v${VER}-src.tar.gz" -o "$f.proof" | grep -q "require-both" || { echo "✗ sign failed $t"; exit 1; }
  "$FLUXC" verify-proof "$f" "$f.proof" | grep -q "✓ require-both" || { echo "✗ verify failed $t"; exit 1; }
done
LB3=$(b3sum "$S/sigil-top-v${VER}-linux-x64" | awk '{print $1}'); LSZ=$(stat -c %s "$S/sigil-top-v${VER}-linux-x64")
WB3=$(b3sum "$S/sigil-top-v${VER}-windows-x64.exe" | awk '{print $1}'); WSZ=$(stat -c %s "$S/sigil-top-v${VER}-windows-x64.exe")

echo "▸ 4/7 publish to $DL (+ legacy channel)"
cp "$S"/sigil-top-v${VER}-* "$DL/"
# LEGACY CHANNEL (2026-07-21): clients from the v7.0.1 era poll quillon.xyz/downloads/ —
# that manifest went stale at v2.0.0 on Jun 17, so old nodes believed they were up to
# date FOREVER. Every release publishes binaries+manifest+sig to BOTH roots now.
LEGACY_DL="/home/orobit/q-narwhalknight/dist-final/downloads"
cp "$S"/sigil-top-v${VER}-* "$LEGACY_DL/"

echo "▸ 5/7 write + SIGN manifest (mandatory — updater fails closed without a valid .sig)"
REV=$(git rev-parse --short HEAD)
cat > "$DL/sigil-top-latest.json" <<EOF
{
  "product": "sigil-top", "version": "$VER", "channel": "stable",
  "url": "$BASE/sigil-top-v${VER}-linux-x64", "blake3_hex": "$LB3", "size_bytes": $LSZ,
  "flux_rev": "$REV", "source_tag": "v$VER",
  "verify": "fluxc verify-proof <artifact> <artifact>.proof   (require-both SQIsign-L5 + Ed25519)",
  "builder_keys": "$BASE/sigil-builder-keys.json",
  "source_archive": "$BASE/sigil-top-v${VER}-src.tar.gz",
  "notes": "$NOTE",
  "targets": {
    "linux-x64":       { "url": "$BASE/sigil-top-v${VER}-linux-x64",       "proof_url": "$BASE/sigil-top-v${VER}-linux-x64.proof",       "blake3_hex": "$LB3", "size_bytes": $LSZ },
    "windows-x64":     { "url": "$BASE/sigil-top-v${VER}-windows-x64.exe", "proof_url": "$BASE/sigil-top-v${VER}-windows-x64.exe.proof", "blake3_hex": "$WB3", "size_bytes": $WSZ },
    "windows-x64-gpu": { "url": "$BASE/sigil-top-v${VER}-windows-x64.exe", "proof_url": "$BASE/sigil-top-v${VER}-windows-x64.exe.proof", "blake3_hex": "$WB3", "size_bytes": $WSZ }
  }
}
EOF
bash scripts/sign-manifest.sh "$DL/sigil-top-latest.json"
# legacy channel gets the identical signed manifest
cp "$DL/sigil-top-latest.json" "$DL/sigil-top-latest.json.sig" "$LEGACY_DL/"

echo "▸ 6/7 verify LIVE manifest+sig against pinned key"
python3 - "$PINNED_PUB" <<'PY'
import urllib.request, time, json, sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
pin=sys.argv[1]; t=int(time.time())
g=lambda u: urllib.request.urlopen(f'https://sigilgraph.fluxapp.xyz/downloads/{u}?t={t}',timeout=25).read()
body=g('sigil-top-latest.json'); sig=bytes.fromhex(g('sigil-top-latest.json.sig').decode().strip()); d=json.loads(body)
Ed25519PublicKey.from_public_bytes(bytes.fromhex(pin)).verify(sig, body)
print('  ✓ live manifest+sig VALID — channel now', d['version'])
PY

echo "▸ 7/7 commit + tag + push"
git add crates/sigil-top/Cargo.toml Cargo.lock
git commit --no-gpg-sign -m "release(sigil-top): v${VER}

$NOTE

  linux-x64   blake3 $LB3
  windows-x64 blake3 $WB3

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>" || echo "  (nothing new to commit besides version — continuing)"
git tag -a "v${VER}" -m "sigil-top v${VER}" 2>/dev/null || echo "  (tag v${VER} exists)"
BRANCH=$(git rev-parse --abbrev-ref HEAD)
git push origin "$BRANCH" "v${VER}"

# refresh the code-provenance page the explorer's flux-rev tab iframes (post-tag so
# the new release row appears; non-fatal — the page just lags one release on failure)
bash scripts/gen-flux-versions.sh || echo "  (flux-versions refresh failed — non-fatal)"

echo ""
echo "✅ RELEASED sigil-top v${VER} — channel live, signed, tagged. Nodes pull it on next update check."
echo "   linux   $LB3"
echo "   windows $WB3"
