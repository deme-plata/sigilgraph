#!/usr/bin/env bash
# SIGIL cross-node integrity check.
#
# Two nodes are redundant only if disagreement is DETECTABLE. This polls both until they report
# the same height, then compares everything the header commits to.
#
# The important one is wallet_state_root: it is an additive multiset accumulator over every live
# (wallet, token) -> amount entry, so equal roots at equal height means every balance matches —
# all of them, not a sample. There is no need to enumerate wallets and diff them.
#
# Comparing at DIFFERENT heights proves nothing. The roots roll forward every block, so two
# honest nodes one block apart legitimately differ. This script refuses to compare until the
# heights line up, which is the mistake it exists to prevent.
#
# Usage: sigil-integrity-check.sh [nodeA_url] [nodeB_url] [max_polls]
set -uo pipefail
A="${1:-http://127.0.0.1:18181}"
B="${2:-http://10.77.0.5:18181}"
MAX="${3:-60}"

get() { curl -s --max-time 10 "$1/v1/integrity" 2>/dev/null; }
field() { python3 -c "import sys,json;d=json.load(sys.stdin);print(d$2)" 2>/dev/null <<<"$1"; }

echo "SIGIL cross-node integrity"
echo "  A: $A"
echo "  B: $B"
echo

for i in $(seq 1 "$MAX"); do
  ja=$(get "$A"); jb=$(get "$B")
  [ -z "$ja" ] && { echo "A did not answer"; exit 2; }
  [ -z "$jb" ] && { echo "B did not answer"; exit 2; }
  ha=$(field "$ja" "['height']"); hb=$(field "$jb" "['height']")
  if [ "$ha" = "$hb" ] && [ -n "$ha" ]; then
    echo "matched at height $ha — comparing"
    echo
    fail=0
    for k in "['roots']['wallet_state_root']" "['roots']['dex_state_root']" \
             "['roots']['event_log_root']" "['roots']['contract_state_root']" \
             "['native_supply']" "['shielded']['anchor']" "['shielded']['notes']" \
             "['shielded']['nullifiers']"; do
      va=$(field "$ja" "$k"); vb=$(field "$jb" "$k")
      name=$(sed "s/\]\['/./g; s/\['//; s/'\]//" <<<"$k")
      if [ "$va" = "$vb" ]; then
        printf "  AGREE     %-22s %s\n" "$name" "${va:0:32}"
      else
        printf "  DISAGREE  %-22s A=%s B=%s\n" "$name" "${va:0:24}" "${vb:0:24}"
        fail=1
      fi
    done
    echo
    if [ "$fail" -eq 0 ]; then
      echo "RESULT: the two nodes agree on every committed value at height $ha."
      echo "        wallet_state_root matching means EVERY wallet balance matches."
      exit 0
    fi
    echo "RESULT: FORK — the nodes disagree at the SAME height. Stop and investigate."
    exit 1
  fi
  echo "  heights differ (A=$ha B=$hb) — waiting for them to meet [$i/$MAX]"
  sleep 10
done
echo "heights never matched within $MAX polls; the trailing node is still catching up."
exit 3
