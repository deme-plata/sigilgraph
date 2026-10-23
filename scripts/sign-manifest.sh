#!/usr/bin/env bash
# Sign a SIGIL release manifest (sigil-top, wallet, [A]I, skills) with the pinned release key(s).
# Usage: sign-manifest.sh <manifest.json>
#
#   always  -> <manifest.json>.sig2   KEY 2 (a78061cf…25ae, since 2026-10-23). sigil-top >= 10.6.9 and
#                                     the Android wallet >= 1.13.5 read this file and trust only key 2.
#   bridge  -> <manifest.json>.sig    KEY 1 (150fb84d…6402, 2026-06-10). Written ONLY while key 1's seed is
#                                     still on this host. Installs from before the rotation trust only key 1
#                                     (it is compiled in), so this is how they reach a build that trusts
#                                     key 2. Key 1 sat readable on Epsilon during the 2026-09-23 compromise.
#
# Ending the bridge = moving key 1's seed out of /root/.config/sigil (into the incident evidence folder).
# From then on no .sig is written; an old .sig beside a new manifest no longer verifies, so a stray
# pre-rotation install fails CLOSED ("MANIFEST SIGNATURE INVALID") and needs a manual download.
#
# The signature is over the EXACT manifest bytes. Each seed is checked against the public key it must
# have BEFORE it signs, so a wrong or swapped seed file fails here and not at every client.
set -euo pipefail
MANIFEST="${1:?usage: sign-manifest.sh <manifest.json>}"
SEED2="${SIGIL_RELEASE_SEED_V2:-/root/.config/sigil/release-sign-v2.seed}"
SEED1="${SIGIL_RELEASE_SEED:-/root/.config/sigil/release-sign.seed}"
[ -f "$MANIFEST" ] || { echo "✗ manifest not found: $MANIFEST" >&2; exit 1; }
[ -f "$SEED2" ] || { echo "✗ release seed (key 2) not found: $SEED2" >&2; exit 1; }
python3 -I - "$MANIFEST" "$SEED2" "$SEED1" <<'PY'
import os, sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization as s
manifest, seed2, seed1 = sys.argv[1:4]
KEYS = (  # label, seed, suffix, the public key that seed must have, required?
    ("key 2", seed2, ".sig2", "a78061cfb9e2b2cdd5e876da5d737ba0edce6ef77ba3b91e83acb892741225ae", True),
    ("key 1 bridge", seed1, ".sig", "150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402", False),
)
body = open(manifest, "rb").read()
# Check every seed first: a refusal must leave no half-signed manifest behind.
signers = []
for label, seedf, suffix, want, required in KEYS:
    if not os.path.exists(seedf):
        if required:
            sys.exit(f"✗ {label} seed missing: {seedf}")
        print(f"  · {label}: seed not on this host — bridge over, no {suffix} written")
        continue
    sk = Ed25519PrivateKey.from_private_bytes(open(seedf, "rb").read())
    pk = sk.public_key().public_bytes(s.Encoding.Raw, s.PublicFormat.Raw).hex()
    if pk != want:
        sys.exit(f"✗ {seedf} is not {label} (pubkey {pk[:8]}…, want {want[:8]}…) — refusing to sign")
    signers.append((label, sk, suffix, want))
for label, sk, suffix, want in signers:
    sig = sk.sign(body)
    sk.public_key().verify(sig, body)
    out = manifest + suffix
    open(out + ".tmp", "w").write(sig.hex())
    os.replace(out + ".tmp", out)
    print(f"✓ signed {manifest} -> {out} ({len(sig)}B ed25519, {label} {want[:8]}…)")
PY
