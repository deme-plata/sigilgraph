#!/bin/bash
# Local 2-node SIGIL mesh on Delta: a peer (B, :9502) + a producer (A, :9501)
# bootstrapped to B. Production is gated on >=1 peer, so a single node only
# heartbeats — this gives A its peer (B) so it mints, self-contained (no Epsilon).
# Both in-memory (the binary doesn't persist yet — separate flux-db fix).
set -u
BIN=/home/orobit/target-sigil/release/sigil-node
DATA=/home/orobit/sigil-data

pkill -9 -f 'sigil-node start' 2>/dev/null; sleep 2
rm -f "$DATA/delta.log" "$DATA/nodeB.log"
rm -rf "$DATA/db-a" "$DATA/db-b"   # fresh genesis on a clean mesh start

# ── Node B — peer/verifier on :9502. WG mode is the only mode that honors
#    SIGIL_WG_LISTEN_ADDR, so we use it to move B off :9501 (sigilwg0 is up). ──
setsid env SIGIL_NODE_ID=sigil-g0-delta-b SIGIL_TRANSPORT=wireguard:sigilwg0 \
  SIGIL_WG_LISTEN_ADDR=/ip4/0.0.0.0/tcp/9502 SIGIL_SNAPSHOT_DIR="$DATA/db-b" SIGIL_PRODUCER=0 SIGIL_DAG=1 \
  "$BIN" start </dev/null >"$DATA/nodeB.log" 2>&1 & disown

# wait for B to print its libp2p identity
BID=""
for _ in $(seq 1 20); do
  BID=$(grep -oE '12D3Koo[A-Za-z0-9]{40,}' "$DATA/nodeB.log" 2>/dev/null | head -1)
  [ -n "$BID" ] && break
  sleep 1
done
echo "Node B peer-id: ${BID:-<none>}"
if [ -z "$BID" ]; then echo "ABORT: B never logged a peer-id"; tail -5 "$DATA/nodeB.log"; exit 1; fi

# ── Node A — producer on :9501, bootstrapped to local B ──
setsid env SIGIL_NODE_ID=sigil-g0-delta SIGIL_TRANSPORT=direct \
  SIGIL_WG_LISTEN_ADDR=/ip4/0.0.0.0/tcp/9501 SIGIL_SNAPSHOT_DIR="$DATA/db-a" \
  SIGIL_BOOTSTRAP_PEERS="/ip4/127.0.0.1/tcp/9502/p2p/$BID" \
  SIGIL_PRODUCER=1 SIGIL_DAG=1 SIGIL_PRODUCE_MS=40 SIGIL_PRODUCE_GRACE_MS=3000 \
  "$BIN" start </dev/null >"$DATA/delta.log" 2>&1 & disown
echo "Node A (producer) launched → bootstrapped to B"

sleep 8
echo "=== ports listening ==="; ss -tlnp 2>/dev/null | grep -E ':9501|:9502' | sed 's/  */ /g'
echo "=== A state ==="
echo -n "peers: "; grep -oE 'peers=[0-9]+' "$DATA/delta.log" | tail -1
echo -n "blocks minted: "; grep -c '📦' "$DATA/delta.log"
echo -n "height: "; grep -oE 'H=[0-9]+' "$DATA/delta.log" | tail -1
echo "--- A last 3 ---"; tail -3 "$DATA/delta.log" | cut -c1-90
