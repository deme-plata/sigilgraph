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

echo "▸ 2/7 build linux + windows (release)"
"$FLUXC" build --release -p sigil-top
"$FLUXC" build --release -p sigil-top --target x86_64-pc-windows-gnu
LBIN="target/release/sigil-top"; WBIN="target/x86_64-pc-windows-gnu/release/sigil-top.exe"
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

echo "▸ 4/7 publish to $DL"
cp "$S"/sigil-top-v${VER}-* "$DL/"

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

echo ""
echo "✅ RELEASED sigil-top v${VER} — channel live, signed, tagged. Nodes pull it on next update check."
echo "   linux   $LB3"
echo "   windows $WB3"
