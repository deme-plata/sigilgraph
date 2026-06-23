# Chronos sync tuning — sigil-g0 backfill (loop session 2026-06-09)

Goal: diagnose "0 block/sec, flux-p2p not wired" via deterministic chronos sim.
Harness: flux_chronos_run (star-flood, virtual time, seeded=42, reproducible).
Disk headroom: /home/storage 69T free → 50GB test class is safe.

## Redundancy sweep @ 16 nodes, 2000 msgs, 40ms, drop=0.10
| redundancy | delivery% | note |
|---|---|---|
| 1 | 90.0 | == (1-drop). THE 0-blk/s mode: a lost chunk gaps synced_to, forward stalls |
| 2 | 98.9 | == (1-drop^2) |
| 3 | 99.9 | == (1-drop^3) three-nines, no stall |

Law: unique-delivery ≈ 1 - drop^redundancy. Pick redundancy = ceil(log(1-target)/log(drop)).
For drop=0.10, target=0.999 → redundancy=3.

## Mapping to sigil-top (block_sync.rs)
- backfill is request/response chunked (CHUNK blocks); a dropped chunk is re-requested on TIMEOUT,
  so the live system already has a recovery path — but at redundancy=1 gossip the FIRST pass stalls.
- "flux-p2p not wired in" = the real NetworkManager isn't delivering; chronos is the proxy that proves
  the gossip math, isolating tuning from the wiring bug.

## Next iterations (loop)
1. scale msgs toward 50GB-equiv chunk count; confirm redundancy=3 holds + measure wall_ms scaling
2. latency sweep (40/100/250ms) — does higher RTT need deeper inflight window, not more redundancy?
3. wire flux-p2p: the chronos verdict (redundancy=3) becomes the live gossip re-propagation setting

## sigil-top 0.10.0 "sync doesn't work" — chronos diagnosis (loop 2)
Sim: 3 nodes (producer + Delta + Epsilon sinks), 1 CHUNK=2048 msgs, 50ms, drop=0.05, redundancy=1.
Result: 95.1% delivery (201/4096 chunk-blocks lost in single pass).

Two failure layers:
1. TRANSPORT (the literal 0 blk/s): flux-p2p not wired/connected → peers=0 → 0% of chunks arrive →
   contiguous synced_to never leaves `base`. Chronos can't repair the transport, but it PROVES the
   stall isn't the algorithm — connect a peer and chunks flow.
2. ALGORITHM (latent): at redundancy=1 a dropped chunk-block gaps synced_to; recovery depends ENTIRELY
   on block_sync.rs's TIMEOUT re-request. block_sync.rs HAS that path → should recover. So the 0 b/s in
   0.10.0 is transport, not the backfill loop.

flux_sigil_chronos_ci(sigil-chronos): build step FAILED, chronos step OK → the chain-sim crate doesn't
build under the CI harness (path/env). Worth a `flux_combo --package sigil-chronos` to confirm.

Next loop iterations:
- redundancy 1→2→3 over the 3-node mesh: does re-propagation alone close the gap without re-request?
- inspect block_sync.rs: is the flux-p2p NetworkManager actually constructed + dialing SIGIL_BOOTSTRAP_PEERS?
  (the real "not wired" suspect — confirm peers>0 before blaming the loop)
- flux_combo --package sigil-chronos to reproduce the CI build failure.
