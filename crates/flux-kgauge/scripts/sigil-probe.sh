#!/usr/bin/env bash
# Build a flux-kgauge `Observables` document from a live sigil-api node.
#
# This is the chain-specific adapter. It is deliberately outside the crate: the
# gauge itself must never learn what a SIGIL block looks like.
#
#   ./sigil-probe.sh [window_secs] [api_base] > obs.json
#   ./sigil-probe.sh | cargo run --example from_stdin -- sigil
#
# What SIGIL can and cannot supply today:
#   peer_count      <- /v1/network/topology .peer_count           MEASURED
#   local_height    <- /v1/mining/miners .height                  MEASURED
#   mining_*        <- /v1/mining/miners .shares_accepted/.rejects MEASURED
#   window blocks   <- /v1/dagknight/recent                        MEASURED
#   network_height  <- not exposed by any route                    -> 0 = unavailable
#   p2p byte totals <- not exposed by any route                    -> 0 = unavailable
#
# Emitting 0 for the last two is correct: the gauge reads a zero byte-delta and
# a zero network height as "this channel had no data" and reports the reading as
# Partial rather than pretending the node is perfectly in sync.
set -euo pipefail

WINDOW="${1:-60}"
BASE="${2:-http://127.0.0.1:18181}"

sample() {
  local topo miners
  topo=$(curl -sf --max-time 8 "$BASE/v1/network/topology")
  miners=$(curl -sf --max-time 8 "$BASE/v1/mining/miners")
  jq -n --argjson t "$topo" --argjson m "$miners" '
    ($m.data.rejects // [] | map(.[1]) | add // 0) as $rej
    | {
        mining_submitted: (($m.data.shares_accepted // 0) + $rej),
        mining_accepted:  ($m.data.shares_accepted // 0),
        p2p_bytes_in: 0,
        p2p_bytes_out: 0,
        peer_count: ($t.data.peer_count // 0),
        local_height: ($m.data.height // 0),
        network_height: 0
      }'
}

PREV=$(sample)
sleep "$WINDOW"
CUR=$(sample)
RECENT=$(curl -sf --max-time 10 "$BASE/v1/dagknight/recent")

# `producer` arrives as a 32-element byte array; the gauge only ever compares
# producers for equality, so pass it straight through.
jq -n \
  --argjson prev "$PREV" \
  --argjson cur "$CUR" \
  --argjson recent "$RECENT" \
  --argjson win "$WINDOW" '
  {
    previous: $prev,
    current: $cur,
    window_secs: $win,
    window: {
      blocks: ($recent.data.blocks // [] | map({
        height: .height,
        producer: .producer,
        merge_parent_count: ((.merge_parents // []) | length),
        blue_score: .blue_score,
        is_blue: .is_blue
      }))
    },
    # Epsilon + happysrv + the handful of peers actually meshed. State what you
    # know: a Known value keeps Omega_node operational, a Placeholder does not.
    network_size: { kind: "known", n: 8 },
    kappa: 18.0
  }'
