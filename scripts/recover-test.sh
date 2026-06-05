#!/bin/bash
# Kill node A (producer :9501), keep B (peer :9502) holding the chain, restart A
# bootstrapped to B — does A SYNC the chain back over P2P? ("kill node, comes back")
DATA=/home/orobit/sigil-data
BIN=/home/orobit/target-sigil/release/sigil-node
ah(){ grep -oE 'H=[0-9]+' "$DATA/delta.log" 2>/dev/null | tail -1; }
bh(){ grep -oE 'H=[0-9]+' "$DATA/nodeB.log" 2>/dev/null | tail -1; }

echo "BEFORE: A=$(ah)  B=$(bh)"
BID=$(grep -oE '12D3Koo[A-Za-z0-9]{40,}' "$DATA/nodeB.log" | head -1)
APID=$(ss -tlnp 2>/dev/null | grep ':9501 ' | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
echo "killing node A (pid ${APID:-?}) — B stays up with the chain"
[ -n "$APID" ] && kill -9 "$APID" 2>/dev/null
sleep 3

# restart A fresh (in-memory, height 0) bootstrapped to B
setsid env SIGIL_NODE_ID=sigil-g0-delta SIGIL_TRANSPORT=direct \
  SIGIL_WG_LISTEN_ADDR=/ip4/0.0.0.0/tcp/9501 \
  SIGIL_BOOTSTRAP_PEERS="/ip4/127.0.0.1/tcp/9502/p2p/$BID" \
  SIGIL_PRODUCER=1 SIGIL_DAG=1 SIGIL_PRODUCE_MS=40 SIGIL_PRODUCE_GRACE_MS=3000 \
  "$BIN" start </dev/null >"$DATA/delta.log" 2>&1 & disown
echo "A restarted fresh → bootstrapped to B. watching for P2P sync-recovery..."
sleep 14
echo "AFTER:  A=$(ah)  B=$(bh)"
echo "--- sync evidence in A's log ---"
grep -iE 'sync|turbo|caught|adopt|tip|reorg|behind|catch' "$DATA/delta.log" 2>/dev/null | tail -4
echo "--- A last 2 ---"; tail -2 "$DATA/delta.log" | cut -c1-90
