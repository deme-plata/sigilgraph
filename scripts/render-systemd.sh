#!/usr/bin/env bash
# render-systemd.sh — produce host-specific sigil-node.service from template.
#
# Usage:
#   scripts/render-systemd.sh delta   → scripts/sigil-node.service.delta
#   scripts/render-systemd.sh epsilon → scripts/sigil-node.service.epsilon
#
# Per-host bootstrap-peer config goes into the SIGIL_BOOTSTRAP_PEERS env var.
# Today we hardcode the single seed (the OTHER box in the 2-node testnet).
# Once Phase 0 has >2 nodes, add more comma-separated entries.

set -euo pipefail

HOST="${1:-}"
case "$HOST" in
    delta)
        # Delta peers Epsilon's SIGIL listener
        BOOTSTRAP_PEERS="/ip4/89.149.241.126/tcp/9501"
        PROFILE_DELTA='active'
        PROFILE_EPSILON='inactive'
        ;;
    epsilon)
        # Epsilon peers Delta's SIGIL listener
        BOOTSTRAP_PEERS="/ip4/5.79.79.158/tcp/9501"
        PROFILE_DELTA='inactive'
        PROFILE_EPSILON='active'
        ;;
    *)
        echo "usage: $0 <delta|epsilon>" >&2
        exit 2 ;;
esac

WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
TEMPLATE="$WORKSPACE/scripts/sigil-node.service.template"
OUT="$WORKSPACE/scripts/sigil-node.service.$HOST"

sed \
    -e "s|@@HOST@@|$HOST|g" \
    -e "s|@@BOOTSTRAP_PEERS@@|$BOOTSTRAP_PEERS|g" \
    "$TEMPLATE" > "$OUT"

# Uncomment the matching profile block.
if [[ "$PROFILE_EPSILON" == "active" ]]; then
    # Strip leading "# " from the four Epsilon caps lines.
    sed -i 's|^# CPUQuota=200%|CPUQuota=200%|; s|^# MemoryMax=4G|MemoryMax=4G|; s|^# MemoryHigh=3G|MemoryHigh=3G|; s|^# IOWeight=50|IOWeight=50|' "$OUT"
fi

echo "wrote $OUT"
echo
echo "Install on $HOST:"
echo "  scp $OUT root@\$($HOST_IP):/etc/systemd/system/sigil-node.service"
echo "  ssh root@\$($HOST_IP) 'systemctl daemon-reload && systemctl enable --now sigil-node'"
