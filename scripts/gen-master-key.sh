#!/usr/bin/env bash
# gen-master-key.sh — generate the SIGIL master operator keypair.
#
# Mirrors the pattern from `scripts/release.sh`: idempotent, one keypair
# per network, lives at `keys/sigil-master.{sk,pk}.hex`. The PUBLIC key is
# the operational identity that sigil-bank checks for master-authority
# operations (per skill rule about master-wallet ops being multisig in P1+).
#
# The 32-byte `MASTER_WALLET_GENESIS` constant in sigil-node baked into
# block 0 is DETERMINISTIC ([0xAA; 32]) so all nodes mint identical
# genesis. The keypair generated here is SEPARATE — it's what sigil-bank
# verifies operator signatures against. P1+ will substitute the keypair's
# pubkey into the genesis const, but for P0 we keep them decoupled so the
# genesis hash stays stable across operator key rotations.
#
# Run once after `scripts/host-setup.sh` on the genesis-author machine.
# Result goes in the repo's `keys/` dir (which is .gitignored) — back it
# up out-of-band per your operator playbook.

set -euo pipefail

WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
SK="$WORKSPACE/keys/sigil-master.sk.hex"
PK="$WORKSPACE/keys/sigil-master.pk.hex"
UPDATER="$WORKSPACE/target/debug/sigil-updater"

[[ -x "$UPDATER" ]] || { echo "missing $UPDATER — run: fluxc build --package sigil-updater" >&2; exit 1; }

if [[ -f "$SK" && -f "$PK" ]]; then
    echo "✓ sigil-master keypair already exists:"
    echo "    sk: $SK"
    echo "    pk: $PK"
    echo "  (delete both files first to regenerate — but note that any sigil-bank state"
    echo "   tied to the OLD pubkey will reject the new one)"
    exit 0
fi

mkdir -p "$WORKSPACE/keys"
echo "[gen-master-key] generating SIGIL master keypair via sigil-updater keygen"
cd "$WORKSPACE/keys"
"$UPDATER" keygen --out-prefix sigil-master
chmod 600 "$SK"

echo
echo "✓ wrote $SK (chmod 600, hex-encoded SQIsign5 secret key)"
echo "✓ wrote $PK (hex-encoded SQIsign5 public key)"
echo
echo "Back this up out-of-band — losing $SK = losing all master-authority operations on this network."
echo "Add the PUBLIC key (contents of $PK) to your operator runbook."
echo
echo "Note: this keypair is SEPARATE from the genesis MASTER_WALLET_GENESIS const ([0xAA; 32])"
echo "      baked into sigil-node's block 0. P1+ will unify them; P0 keeps them decoupled so"
echo "      operator key rotations don't change the genesis hash."
