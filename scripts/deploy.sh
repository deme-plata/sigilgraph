#!/usr/bin/env bash
# deploy.sh — push a SIGIL release to delta or epsilon and start the service.
#
# Usage:
#   scripts/deploy.sh delta   <version>
#   scripts/deploy.sh epsilon <version>
#
# Prerequisites on the remote host (one-time setup, see scripts/host-setup.sh):
#   - /home/orobit/sigil-data/{bin,db,staging}/ dirs exist
#   - sigil-updater binary at /home/orobit/sigil-data/bin/sigil-updater
#     (deploy the same binary you built locally; trust comes from the .proof)
#   - sigil-node.service file at /etc/systemd/system/ (use render-systemd.sh)
#   - User orobit owns /home/orobit/sigil-data
#
# Flow:
#   1. scp binary + announcement to remote /home/orobit/sigil-data/staging/
#   2. ssh: sigil-updater verify (sig + binary hash check)
#   3. ssh: sigil-updater apply → swaps /home/orobit/sigil-data/bin/sigil-node
#   4. ssh: systemctl restart sigil-node
#   5. ssh: journalctl -u sigil-node --since 10s ago → health check (no panic)
#
# Auto-rollback: if step 5 sees ERROR/panic, restore from .bak and restart.

set -euo pipefail

HOST="${1:-}"
VERSION="${2:-}"
if [[ -z "$HOST" || -z "$VERSION" ]]; then
    echo "usage: $0 <delta|epsilon> <version>" >&2
    exit 2
fi

case "$HOST" in
    delta)   SSH_TARGET="root@5.79.79.158" ;;
    epsilon) SSH_TARGET="root@89.149.241.126" ;;
    *)
        echo "unknown host '$HOST' (expected: delta | epsilon)" >&2
        exit 2 ;;
esac

WORKSPACE="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="$WORKSPACE/releases/v$VERSION"
BIN_NAME="sigil-node-v$VERSION"
BIN_PATH="$OUT_DIR/$BIN_NAME"
ANN_PATH="$OUT_DIR/$BIN_NAME.announcement.json"

for f in "$BIN_PATH" "$ANN_PATH"; do
    [[ -f "$f" ]] || { echo "missing $f — run scripts/release.sh $VERSION first" >&2; exit 1; }
done

REMOTE_STAGING="/home/orobit/sigil-data/staging"
REMOTE_TARGET="/home/orobit/sigil-data/bin/sigil-node"

echo "[deploy] 1/5 scp → $SSH_TARGET:$REMOTE_STAGING/"
ssh "$SSH_TARGET" "mkdir -p $REMOTE_STAGING"
scp -q "$BIN_PATH" "$ANN_PATH" "$SSH_TARGET:$REMOTE_STAGING/"

echo "[deploy] 2/5 remote verify (sig + binary hash)"
ssh "$SSH_TARGET" "\
    /home/orobit/sigil-data/bin/sigil-updater verify \
        --announcement $REMOTE_STAGING/$BIN_NAME.announcement.json \
        --binary $REMOTE_STAGING/$BIN_NAME \
" | sed 's/^/  /'

echo "[deploy] 3/5 remote apply (atomic .new → target → .bak swap)"
ssh "$SSH_TARGET" "\
    /home/orobit/sigil-data/bin/sigil-updater apply \
        --announcement $REMOTE_STAGING/$BIN_NAME.announcement.json \
        --binary $REMOTE_STAGING/$BIN_NAME \
        --target $REMOTE_TARGET \
" | sed 's/^/  /'

echo "[deploy] 4/5 systemctl restart sigil-node"
ssh "$SSH_TARGET" "systemctl restart sigil-node && systemctl is-active sigil-node"

echo "[deploy] 5/5 health check (10s journal)"
sleep 5
HEALTH=$(ssh "$SSH_TARGET" "journalctl -u sigil-node --since '15 seconds ago' --no-pager 2>&1")
echo "$HEALTH" | sed 's/^/  /'
if echo "$HEALTH" | grep -qiE 'panic|fatal|preflight_fail'; then
    echo
    echo "✗ health check failed — rolling back"
    ssh "$SSH_TARGET" "\
        if [[ -f $REMOTE_TARGET.bak ]]; then \
            mv $REMOTE_TARGET.bak $REMOTE_TARGET; \
            systemctl restart sigil-node; \
            echo 'rolled back to previous binary'; \
        else \
            echo 'no .bak to roll back to'; \
        fi \
    "
    exit 1
fi

echo
echo "✓ deployed sigil-node v$VERSION to $HOST ($SSH_TARGET)"
echo "  monitor: ssh $SSH_TARGET 'journalctl -u sigil-node -f'"
