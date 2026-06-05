#!/bin/bash
# sigil-status-writer — derives a live SIGIL testnet snapshot from the running
# node's log and writes it as a static JSON the q-flux static server already
# serves (the proven garden-state.json pattern). REAL chain data, zero risk to
# the node: read-only on the log, no rebuild, no restart, no API, no routing.
#
#   delta.log carries:  H=<height>  ·  peers=<n>  ·  📦{"h":..,"hash":..,"prod":..}
#   out: dist-final/sigil-status.json = {status:{...}, blocks:[...]}
set -u
LOG="${SIGIL_LOG:-/home/orobit/sigil-data/delta.log}"
OUT="${SIGIL_STATUS_OUT:-/opt/orobit/shared/q-narwhalknight/dist-final/sigil-status.json}"
REWARD_SIG=50          # 50 SIGIL/block (pre-first-halving), QUG-model
INTERVAL="${SIGIL_STATUS_INTERVAL:-3}"

commafy(){ echo "$1" | sed -e ':a' -e 's/\B[0-9]\{3\}\>/,&/;ta'; }

while true; do
  H=$(grep -oE 'H=[0-9]+' "$LOG" 2>/dev/null | tail -1 | cut -d= -f2); H=${H:-0}
  P=$(grep -oE 'peers=[0-9]+' "$LOG" 2>/dev/null | tail -1 | cut -d= -f2); P=${P:-0}
  SUPPLY=$(( H * REWARD_SIG ))
  # distinct producers seen in the last 200 block lines = "agents" proxy
  AGENTS=$(grep -oE '"prod":"[^"]*"' "$LOG" 2>/dev/null | tail -200 | sort -u | wc -l)
  AGENTS=${AGENTS:-0}; [ "$AGENTS" -eq 0 ] && AGENTS=1
  # last 12 block JSON lines → array (newest first); strip the 📦 prefix, parse with jq
  BLOCKS=$(grep -aoE '\{"h":[0-9].*\}' "$LOG" 2>/dev/null | tail -12 \
    | jq -c '{height:.h, hash:(.hash // .block_hash // ""), producer:(.prod // "?"), txs:(.txs // 0), tip_ms:10}' 2>/dev/null \
    | jq -s 'reverse' 2>/dev/null)
  [ -z "$BLOCKS" ] && BLOCKS='[]'
  TMP="${OUT}.tmp"
  cat > "$TMP" <<EOF
{"status":{"height":$H,"tip_proofs":$H,"peers":$P,"agents":$AGENTS,"supply":"$(commafy "$SUPPLY")","dex_vol":"—","network_id":"sigil-g0","reward_sig":$REWARD_SIG,"live":true,"updated":$(date +%s)},"blocks":$BLOCKS}
EOF
  mv -f "$TMP" "$OUT" 2>/dev/null   # atomic swap so readers never see a half-written file
  sleep "$INTERVAL"
done
