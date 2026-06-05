# SIGIL Lightweight Node — Prototype v0 Plan (2026-06-01)

> A node that **trustlessly follows the chain without storing it** — verifies a
> multi-producer-signed tip, backfills only the gap it needs, and earns SIGIL for
> verifying. The browser-grade citizen of the SIGIL fabric.

## The one-line insight from the propagation work
**Everything for a light node already exists *except one primitive.*** The cross-WAN
saga proved the wall is **gossip-has-no-history**: a late-joining node sees live blocks
at `H=166+` that can't chain to its genesis `H=1` → rejects all (this was Delta's
`apply H=166 -> Rejected`). The *fix* is already **proven in simulation**
(`sigil-chronos/turbosync.rs::run_late_join_backfill` — 32 tests green): backfill the
gap, then apply live. What's missing is the **wire to fetch that gap** — a p2p
request/response block-range protocol. flux-p2p today has only gossipsub + kademlia +
identify (verified). That gap-fetch is the whole prototype.

## What already exists (don't rebuild)
| Piece | Crate | State |
|---|---|---|
| Block apply + 4-root verify chokepoint | `sigil-chronos` (`apply_external_block`) | ✅ |
| Batch backfill + sync-down guard | `sigil-chronos/turbosync.rs`, `flux-turbo-sync` | ✅ sim-proven |
| Late-join backfill *design proof* | `turbosync::run_late_join_backfill` | ✅ 32 tests |
| Tip-proof light verify | `sigil-tip-proof` (+ `observatory.rs`) | ✅ |
| Signature verify | `sigil-sigverify` | ✅ |
| Unique-identity p2p (the peer-collapse fix) | `sigil-chronos-net.rs` (`node_id`+pid) | ✅ shipped |
| Real cross-host fabric | Epsilon↔Delta WireGuard, 0.25ms RTT | ✅ live |
| **Block-range request/response over p2p** | flux-p2p | ⛔ **MISSING — the build** |

## The build (5 steps, each verify-first)

```
 light node joins late
        │ 1. sees live tip via gossip (H=tip)
        ▼
 2. detect gap: local_height < tip_height
        │ 3. REQUEST H=local+1..tip  ◄── NEW flux-p2p request_response protocol
        ▼
 4. apply gap via turbosync batch path (already proven) → reach tip
        │ 5. verify tip signed by ≥2 producers (sigil-tip-proof + sigverify)
        ▼
   light node at tip, trustless, no full storage → earns SIGIL for verifying
```

1. **flux-p2p: add a `request_response` behaviour** — `BlockRange` protocol:
   `Req{from_height, to_height}` → `Resp{Vec<Block>}` (cap batch size). This is the
   core lane: a 4th `NetworkBehaviour` member alongside gossipsub/kad/identify, with a
   producer-side responder that serves ranges from its store. *Predict-before-build*
   (new behaviour = cold flux-p2p compile; run `flux_architect_predict` + `flux_heatmap`).
2. **sigil-node `--light` mode: backfill-on-graft** — on first live block above local
   height, request the gap, feed it through the existing turbosync batch apply, then
   resume the live stream. This turns the *proven-in-sim* fix into a real
   `blocks_applied>0` cross-WAN — the exact metric the propagation goal needs.
3. **Multi-producer tip trust** — accept a tip only if signed by **≥2 distinct
   producers** (the multi-producer DAG the stargate render shows). Wire
   `sigil-tip-proof` + `sigil-sigverify`; reject single-producer tips (Sybil floor).
4. **Light-verifier credit (balance-safe, sim-first)** — a light node that verifies a
   tip + reports earns SIGIL (the P1 `light-miner-wallet-credit` lane). **Max-wins
   balance writes only** (the Quillon postmortem rule), and **sim-first** before any
   real credit.
5. **Package tiny** — a slim native `sigil-light` binary **and** the WASM verifier
   (`sigil-tip-wasm`) so it runs in-browser under QuillonOS. Static-musl for Vast/fleet.

## Exit criteria (the prototype is "done" when)
| # | Criterion | How verified |
|---|---|---|
| E1 | Late-joiner reaches the live tip | `blocks_applied>0, divergence=0` cross-WAN (was 0/reject) |
| E2 | Backfill is correct | turbosync 4-root match on every backfilled block |
| E3 | Tip trust holds | single-producer tip rejected; ≥2-producer tip accepted |
| E4 | No full-chain storage | light node holds only headers + the verified tip (not all blocks) |
| E5 | Verifier earns SIGIL | sim-first credit, max-wins, settles to a wallet |
| E6 | Runs in browser | `sigil-tip-wasm` verifies a signed tip in WASM (QuillonOS) |

## Discipline (binding)
- **chronos-sim-first**: extend `run_late_join_backfill` to model the request/response
  round-trip *before* the real flux-p2p build. Sim proves the design for $0.
- **unique node_id per process** (the peer-collapse fix) — every light instance.
- **FLUXFOOD** + `fluxc` only (no raw cargo); **predict-before-build** the flux-p2p lane.
- **verify-don't-trust**: read `blocks_applied` from a run, never claim it.
- Test on the **real Epsilon↔Delta WG fabric** (0.25ms RTT, proven) after sim, not just loopback.

## First lane to claim
**`flux-p2p request_response BlockRange protocol`** — it unblocks E1–E4. Everything
else composes on top. Settlement-worthy (the propagation goal's missing keystone).
Companion: [[DEEPSEEK_R1_VAST_RESULTS]], [[FLUX_LLM_STANDARD]], and the
`project_sigil_propagation_loopback_verdict` memory (the root-cause record).
