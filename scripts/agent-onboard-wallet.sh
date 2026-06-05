#!/usr/bin/env bash
# agent-onboard-wallet.sh — give a new swarm agent a SPENDABLE wallet.
#
# THE BUG THIS WORKS AROUND (found 2026-05-30, update-v1):
#   The Quillon Wallet MCP `create_wallet` tool returns an address + "save the
#   mnemonic offline" — but it NEVER actually surfaces the mnemonic/seed in its
#   response. So every wallet created via that tool is UNSPENDABLE (no seed to
#   sign with). This silently blocked the whole "agents join the ecosystem"
#   vision: a new agent would get an address, receive QUG to it, and never be
#   able to move it.
#
# THE FIX (proven — same seed derives the same qnk address deterministically):
#   A Quillon/SIGIL wallet IS just a 64-hex seed. Generate one locally, hand it
#   to the MCP via QNK_SEED / QNK_SEED_FILE, and the MCP's wallet_auth.deriveKeys
#   turns it into a spendable wallet. The agent OWNS the seed (it's a file they
#   control + can back up), can sign from it, and can recover it on any node.
#
# Usage:
#   scripts/agent-onboard-wallet.sh <agent-name>
#     → creates ~/.quillon/seeds/<agent-name>.seed (chmod 600)
#     → prints the derived qnk address
#     → that address is immediately fundable + spendable by that agent
#
# The agent then registers in the swarm with the printed address:
#   flux_swarm_register <agent-name> <qnk-address>

set -euo pipefail

AGENT="${1:-}"
[[ -n "$AGENT" ]] || { echo "usage: $0 <agent-name>" >&2; exit 2; }

SEED_DIR="${QNK_SEED_DIR:-$HOME/.quillon/seeds}"
SEED_FILE="$SEED_DIR/$AGENT.seed"
MCP="${QUILLON_MCP:-/root/.quillon/mcp/build/index.js}"
MCP_CALL="${MCP_CALL:-/tmp/mcp-call.mjs}"

mkdir -p "$SEED_DIR"

if [[ -f "$SEED_FILE" ]]; then
    echo "✓ seed already exists for '$AGENT' at $SEED_FILE (not overwriting)"
else
    # 32 bytes = 64 hex chars = the canonical Quillon seed length.
    # CRITICAL: use /dev/urandom, NOT `openssl rand`. On this sandbox (verified
    # 2026-05-30) `openssl rand -hex 32` returns a DETERMINISTIC value (same
    # bytes every call — mocked/broken RNG). A seed from it is NOT secret:
    # anyone on a similar box derives the same seed and can drain the wallet.
    # /dev/urandom is the real kernel CSPRNG and produces distinct values.
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$SEED_FILE"
    chmod 600 "$SEED_FILE"
    # Sanity: must be exactly 64 hex chars.
    LEN=$(tr -d '\n' < "$SEED_FILE" | wc -c)
    if [[ "$LEN" -ne 64 ]]; then
        echo "✗ seed generation produced $LEN hex chars, expected 64 — aborting" >&2
        rm -f "$SEED_FILE"; exit 1
    fi
    echo "✓ generated fresh 64-hex seed (via /dev/urandom) → $SEED_FILE (chmod 600, NOT printed)"
fi

# Derive the address by handing the seed to the MCP (read-only — derivation is
# deterministic, no on-chain write). Requires the tiny stdio client at $MCP_CALL.
#
# CRITICAL: use QNK_SEED_FILE, NOT QNK_SEED. The MCP's loadSeed precedence
# (wallet_auth.js:70) is: 1.seedArg → 2.QNK_SEED_FILE → 3.~/.claude/quillon-agent-seed
# → 4.QNK_SEED. Since rocky's seed lives at #3, QNK_SEED (#4) NEVER wins — it
# would silently derive rocky's address for every agent. QNK_SEED_FILE (#2)
# beats the rocky default, so each agent gets THEIR own wallet. (Verified
# 2026-05-30: same seed file → same fresh address deterministically, distinct
# from rocky's.)
if [[ -x "$(command -v node)" && -f "$MCP_CALL" ]]; then
    ADDR=$(QNK_SEED_FILE="$SEED_FILE" node "$MCP_CALL" tools/call wallet_info '{}' 2>/dev/null \
        | node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{const m=s.match(/qnk[0-9a-f]{64}/);console.log(m?m[0]:'(derive failed)')})")
    echo "✓ derived address: $ADDR"
    echo
    echo "Next steps for agent '$AGENT':"
    echo "  1. Register:  flux_swarm_register $AGENT $ADDR"
    echo "  2. To sign from this wallet, set QNK_SEED_FILE=$SEED_FILE before MCP calls (NOT QNK_SEED — it loses to rocky's default file)"
    echo "  3. Back up $SEED_FILE out-of-band — losing it = losing the wallet"
    echo "  4. Ask the payer (currently update-v1, rocky wallet) to send your QUG to $ADDR"
else
    echo "⚠ node or $MCP_CALL not available — seed created but address not derived."
    echo "  Derive manually: QNK_SEED=\$(cat $SEED_FILE) <mcp> wallet_info"
fi
