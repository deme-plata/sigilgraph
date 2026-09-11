#!/usr/bin/env bash
# SIGIL cross-node integrity check.
#
# Two nodes are redundant only if disagreement would be DETECTABLE. This compares what both
# nodes publish about the same chain and reports agreement, disagreement, or "cannot tell yet" —
# never a guess.
#
# IT CHECKS TWO THINGS, and the order matters.
#
# 1. THE FINALITY CERTIFICATE (authoritative). Each node publishes the certificate it settled on:
#    a height, the spine block hash at that height, and the order hash — the canonical ordering
#    of the DAG up to it. That triple is internally consistent by construction: all three come
#    from one certificate, so there is no chance of pairing a hash with the wrong height. If both
#    nodes name the same height and the same two hashes, they agree on the finalized chain and on
#    the order of everything in it. This is the real proof.
#
# 2. THE LIVE STATE ROOTS (supporting). /v1/integrity reports the four header roots, supply and
#    shielded-pool state. wallet_state_root is an additive multiset accumulator over every live
#    (wallet, token) -> amount entry, so equal roots at equal height means EVERY balance matches,
#    not a sample. But the height and the roots come from two different reads of a chain moving
#    at several blocks a second, so the heights often will not line up exactly. When they do, the
#    comparison is reported; when they do not, that is stated rather than papered over.
#
# Usage: sigil-integrity-check.sh [nodeA_url] [nodeB_url] [max_polls]
set -uo pipefail
A="${1:-http://127.0.0.1:18181}"
B="${2:-http://10.77.0.5:18181}"
MAX="${3:-30}"

j() { python3 -c "import sys,json
try: d=json.load(sys.stdin)
except Exception: print(''); raise SystemExit
try:
    v=d$1
    print('' if v is None else v)
except Exception: print('')" 2>/dev/null; }

cert() { curl -s --max-time 10 "$1/v1/finality/certificate" 2>/dev/null; }
integ() { curl -s --max-time 10 "$1/v1/integrity" 2>/dev/null; }

echo "SIGIL cross-node integrity"
echo "  A: $A"
echo "  B: $B"
echo

# ── 1. finality certificate — the authoritative comparison ────────────────────────────────
echo "[1] finality certificate (authoritative: height + spine hash + order hash, one consistent triple)"
ok_cert=0
for i in $(seq 1 "$MAX"); do
  ca=$(cert "$A"); cb=$(cert "$B")
  ha=$(j "['certificate']['height']" <<<"$ca"); hb=$(j "['certificate']['height']" <<<"$cb")
  if [ -z "$ha" ] || [ -z "$hb" ]; then
    echo "  a node has not settled a certificate yet (A=${ha:-none} B=${hb:-none}) — cannot tell [$i/$MAX]"
    sleep 10; continue
  fi
  if [ "$ha" != "$hb" ]; then
    echo "  certificates are at different heights (A=$ha B=$hb) — waiting [$i/$MAX]"
    sleep 10; continue
  fi
  sa=$(j "['certificate']['spine_block_hash']" <<<"$ca"); sb=$(j "['certificate']['spine_block_hash']" <<<"$cb")
  oa=$(j "['certificate']['order_hash']" <<<"$ca");       ob=$(j "['certificate']['order_hash']" <<<"$cb")
  echo "  both settled at height $ha"
  if [ "$sa" = "$sb" ] && [ "$oa" = "$ob" ]; then
    echo "    AGREE  spine_block_hash  ${sa:0:32}"
    echo "    AGREE  order_hash        ${oa:0:32}"
    echo "  => the two nodes agree on the finalized chain AND on the canonical order within it."
    ok_cert=1
  else
    echo "    DISAGREE  spine A=${sa:0:24} B=${sb:0:24}"
    echo "    DISAGREE  order A=${oa:0:24} B=${ob:0:24}"
    echo "  => FORK at the same height. Stop and investigate."
    exit 1
  fi
  break
done
[ "$ok_cert" -eq 0 ] && echo "  (certificates never lined up within $MAX polls)"
echo

# ── 2. live state roots — supporting, best effort ─────────────────────────────────────────
echo "[2] live state roots (supporting: only comparable when the applied heights happen to meet)"
for i in $(seq 1 "$MAX"); do
  ia=$(integ "$A"); ib=$(integ "$B")
  ha=$(j "['height']" <<<"$ia"); hb=$(j "['height']" <<<"$ib")
  if [ -z "$ha" ] || [ -z "$hb" ]; then
    echo "  a node does not know its applied height yet — refusing to compare [$i/$MAX]"
    sleep 8; continue
  fi
  if [ "$ha" != "$hb" ]; then
    echo "  applied heights differ (A=$ha B=$hb) — not comparable [$i/$MAX]"
    sleep 8; continue
  fi
  echo "  matched at applied height $ha"
  fail=0
  for k in "['roots']['wallet_state_root']" "['roots']['dex_state_root']" \
           "['roots']['event_log_root']" "['roots']['contract_state_root']" \
           "['native_supply']" "['shielded']['anchor']" "['shielded']['notes']" \
           "['shielded']['nullifiers']"; do
    va=$(j "$k" <<<"$ia"); vb=$(j "$k" <<<"$ib")
    name=$(sed "s/\]\['/./g; s/\['//; s/'\]//" <<<"$k")
    if [ "$va" = "$vb" ]; then printf "    AGREE     %-22s %s\n" "$name" "${va:0:32}"
    else printf "    DISAGREE  %-22s A=%s B=%s\n" "$name" "${va:0:24}" "${vb:0:24}"; fail=1; fi
  done
  [ "$fail" -eq 0 ] && echo "  => wallet_state_root matching means EVERY wallet balance matches."
  break
done
echo
[ "$ok_cert" -eq 1 ] && { echo "RESULT: the nodes agree on the finalized chain."; exit 0; }
echo "RESULT: could not establish agreement within $MAX polls (no fork was observed either)."
exit 3
