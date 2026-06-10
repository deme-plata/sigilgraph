#!/usr/bin/env bash
# LANE-C: sign a sigil-top release manifest with the pinned release ed25519 key.
# Usage: sign-manifest.sh <manifest.json>   -> writes <manifest.json>.sig (128-hex ed25519).
# The signature is over the EXACT manifest bytes; the auto-updater verifies it against the pinned
# RELEASE_SIGN_PUBKEY_HEX before trusting any field. Seed lives ONLY on the release host (600).
set -euo pipefail
MANIFEST="${1:?usage: sign-manifest.sh <manifest.json>}"
SEED="${SIGIL_RELEASE_SEED:-/root/.config/sigil/release-sign.seed}"
[ -f "$SEED" ] || { echo "✗ release seed not found: $SEED" >&2; exit 1; }
python3 - "$MANIFEST" "$SEED" <<'PY'
import sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
manifest, seedf = sys.argv[1], sys.argv[2]
body = open(manifest,'rb').read()
sk = Ed25519PrivateKey.from_private_bytes(open(seedf,'rb').read())
sig = sk.sign(body)
open(manifest + '.sig','w').write(sig.hex())
print(f"✓ signed {manifest} -> {manifest}.sig ({len(sig)}B ed25519)")
PY
