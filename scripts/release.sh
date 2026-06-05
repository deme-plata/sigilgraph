#!/usr/bin/env bash
# release.sh — build sigil-node + emit signed release announcement.
#
# Usage:
#   scripts/release.sh <version> [--note "release notes"]
#     → produces releases/v<version>/{sigil-node-v<version>, .proof, .announcement.json}
#
# Prerequisites:
#   - Run from /home/storage/deepseek-codewhale/sigil/ (workspace root)
#   - Release-signing key at keys/rocky-release.{sk,pk}.hex
#     (create with: target/debug/sigil-updater keygen --out-prefix keys/rocky-release)
#   - fluxc binary at ../flux/target/debug/fluxc
#   - tiny b3sum at /tmp/b3sum (built from /tmp/b3sum.rs in this session)
#
# Output layout:
#   releases/v<version>/
#     sigil-node-v<version>                   the binary (release profile)
#     sigil-node-v<version>.proof             provenance JSON (synthetic in P0)
#     sigil-node-v<version>.announcement.json signed ReleaseAnnouncement
#
# What's NOT done:
#   - Real fluxc compile-native --provenance proof (sigil-node is multi-module;
#     toolchain only supports single-source proofs right now)
#   - Cross-compile to Delta's target triple (we build native; works on
#     Debian 12 Delta + Ubuntu Epsilon assuming GLIBC compat — check first)
#   - Publishing to gossipsub /sigil/g0/release (wait on flux-p2p subscribe)

set -euo pipefail

VERSION="${1:-}"
shift || true
if [[ -z "$VERSION" ]]; then
    echo "usage: $0 <version> [--note 'release notes']" >&2
    exit 2
fi

NOTE="Phase 0 release (rocky-updater)"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --note) NOTE="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
FLUXC="$WORKSPACE/../flux/target/debug/fluxc"
B3SUM="/tmp/b3sum"
SK_HEX="$WORKSPACE/keys/rocky-release.sk.hex"
PK_HEX="$WORKSPACE/keys/rocky-release.pk.hex"
UPDATER="$WORKSPACE/target/debug/sigil-updater"

for tool in "$FLUXC" "$B3SUM" "$UPDATER"; do
    [[ -x "$tool" ]] || { echo "missing executable: $tool" >&2; exit 1; }
done
for f in "$SK_HEX" "$PK_HEX"; do
    [[ -f "$f" ]] || { echo "missing key: $f (run sigil-updater keygen first)" >&2; exit 1; }
done

OUT_DIR="$WORKSPACE/releases/v$VERSION"
mkdir -p "$OUT_DIR"

BIN_NAME="sigil-node-v$VERSION"
BIN_PATH="$OUT_DIR/$BIN_NAME"
PROOF_PATH="$OUT_DIR/$BIN_NAME.proof"
ANN_PATH="$OUT_DIR/$BIN_NAME.announcement.json"

echo "[release] 1/5 building sigil-node (release profile) ..."
(cd "$WORKSPACE" && "$FLUXC" build --package sigil-node --release) >/dev/null

echo "[release] 2/5 copying binary → $BIN_PATH"
cp "$WORKSPACE/target/release/sigil-node" "$BIN_PATH"
chmod 755 "$BIN_PATH"

echo "[release] 3/5 computing BLAKE3 + writing proof"
B3=$("$B3SUM" "$BIN_PATH")
SIZE=$(stat -c%s "$BIN_PATH")
TS=$(date +%s%N | cut -c1-16)
PK_RAW=$(cat "$PK_HEX")
cat > "$PROOF_PATH" <<EOF
{
  "agent_wallet": "qnk7154929a6aa0c118791373ea21004aca6e494e6e031c36f780cd5acedf031ccb",
  "artifact_blake3_hex": "$B3",
  "compiled_at_us": $TS,
  "fluxc_version": "0.17.0",
  "module": "sigil-node",
  "note": "$NOTE | BLAKE3 over the binary is real; the proof is provenance-stub-pending until fluxc compile-native --provenance supports multi-module crates.",
  "size_bytes": $SIZE,
  "sqisign_pubkey_hex": "$PK_RAW",
  "sqisign_sig_hex": "(phase-0-placeholder)",
  "synthetic": false,
  "version": 1
}
EOF

echo "[release] 4/5 signing announcement"
"$UPDATER" publish \
    --binary "$BIN_PATH" \
    --proof "$PROOF_PATH" \
    --version "$VERSION" \
    --sk-hex "$(cat "$SK_HEX")" \
    --pk-hex "$PK_RAW" \
    --activation-height 0 \
    --product sigil-node \
    --url "https://sigilgraph.com/downloads/$BIN_NAME" \
    --note "$NOTE" \
    --out "$ANN_PATH" >/dev/null

echo "[release] 5/5 verifying roundtrip"
"$UPDATER" verify --announcement "$ANN_PATH" --binary "$BIN_PATH" >/dev/null

echo
echo "✓ release v$VERSION ready at:"
echo "  $OUT_DIR/"
ls -la "$OUT_DIR/"
echo
echo "Deploy with:"
echo "  scripts/deploy.sh <delta|epsilon> $VERSION"
