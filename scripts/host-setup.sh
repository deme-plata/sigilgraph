#!/usr/bin/env bash
# host-setup.sh — bootstrap a SIGIL host before first deploy.
#
# Usage (run ONCE per host, then deploy.sh handles all updates):
#   scripts/host-setup.sh delta
#   scripts/host-setup.sh epsilon
#
# Idempotent — re-running is safe; skips work that's already done.
#
# What it does on the remote host:
#   1. mkdir -p /home/orobit/sigil-data/{bin,db,staging}
#   2. chown -R orobit:orobit /home/orobit/sigil-data
#   3. scp the sigil-updater Linux binary to /home/orobit/sigil-data/bin/
#      (the publisher-signed announcement chain trusts itself; we just need
#       sigil-updater to be there so deploy.sh's `verify + apply` works)
#   4. render + install the host-specific systemd unit (sigil-node.service)
#   5. systemctl daemon-reload (but NOT start — sigil-node binary isn't there
#      yet; that lands on first deploy.sh)
#
# After this runs, scripts/deploy.sh <host> <version> takes over and never
# needs host-setup.sh again.

set -euo pipefail

HOST="${1:-}"
case "$HOST" in
    delta)   SSH_TARGET="root@5.79.79.158" ;;
    epsilon) SSH_TARGET="root@89.149.241.126" ;;
    *)
        echo "usage: $0 <delta|epsilon>" >&2
        exit 2 ;;
esac

WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
UPDATER_LOCAL="$WORKSPACE/target/release/sigil-updater"
if [[ ! -x "$UPDATER_LOCAL" ]]; then
    # Fall back to debug; release isn't always built first.
    UPDATER_LOCAL="$WORKSPACE/target/debug/sigil-updater"
fi
[[ -x "$UPDATER_LOCAL" ]] || { echo "missing sigil-updater binary at $UPDATER_LOCAL — run release.sh or fluxc build --package sigil-updater first" >&2; exit 1; }

echo "[host-setup] 1/5 creating /home/orobit/sigil-data/{bin,db,staging} on $HOST"
ssh "$SSH_TARGET" "\
    sudo -u orobit mkdir -p /home/orobit/sigil-data/{bin,db,staging} && \
    chown -R orobit:orobit /home/orobit/sigil-data && \
    ls -la /home/orobit/sigil-data \
" | sed 's/^/  /'

echo "[host-setup] 2/5 scp sigil-updater binary"
scp -q "$UPDATER_LOCAL" "$SSH_TARGET:/home/orobit/sigil-data/bin/sigil-updater"
ssh "$SSH_TARGET" "chmod 755 /home/orobit/sigil-data/bin/sigil-updater"

echo "[host-setup] 3/5 verify sigil-updater runs on remote"
ssh "$SSH_TARGET" "/home/orobit/sigil-data/bin/sigil-updater version" | sed 's/^/  /'

echo "[host-setup] 4/5 render + install sigil-node.service"
SERVICE_LOCAL="$WORKSPACE/scripts/sigil-node.service.$HOST"
if [[ ! -f "$SERVICE_LOCAL" ]]; then
    echo "  rendering systemd unit for $HOST"
    "$WORKSPACE/scripts/render-systemd.sh" "$HOST" >/dev/null
fi
scp -q "$SERVICE_LOCAL" "$SSH_TARGET:/etc/systemd/system/sigil-node.service"

echo "[host-setup] 5/5 systemctl daemon-reload (NOT starting — wait for first deploy.sh)"
ssh "$SSH_TARGET" "systemctl daemon-reload && systemctl status sigil-node --no-pager 2>&1 | head -5 || true" | sed 's/^/  /'

# Capacity probe — let the operator see what they're working with
echo
echo "[host-setup] capacity probe"
ssh "$SSH_TARGET" "echo '  cpu cores: ' \$(nproc); echo '  total mem: ' \$(free -h | awk '/^Mem:/ {print \$2}'); echo '  disk /home/orobit:'; df -h /home/orobit | tail -1 | awk '{printf \"    %s used, %s avail (%s full)\\n\", \$3, \$4, \$5}'"

# Firewall hint — SIGIL needs :9501 TCP (P2P) + :8181 TCP (API) open
echo
echo "[host-setup] firewall — verify ports open from off-box:"
echo "  P2P:  nc -zv $(echo "$SSH_TARGET" | sed 's/.*@//') 9501"
echo "  API:  nc -zv $(echo "$SSH_TARGET" | sed 's/.*@//') 8181"
echo "  (if closed: open in provider panel + add iptables ACCEPT rules)"

echo
echo "✓ host $HOST is ready. Next step: scripts/deploy.sh $HOST <version>"
