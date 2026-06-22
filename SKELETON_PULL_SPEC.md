# Skeleton instant fast-sync — completion spec (handoff to flux 1)  [2026-06-23]

Baseline: testnet reset to fresh genesis c6b2b549...; producer (epsilon sigil-node 5.0.0)
~25 blk/s via SIGIL_PRODUCE_US; gamma follower converges via full-block rr-backfill (~39/s);
box (sigil-top v6.0.5) mining + earning. The ONLY missing piece for instant light-client sync
is below.

## WORKS (verified in deployed 5.0.0)
- SERVER serves codec=2 skeleton pages: crates/sigil-node/src/main.rs
  :845 serve_cap branches on req.headers_only; :900 out = if headers_only;
  :922 'S' page; :67 'codec=2 -> S + bincode(Vec<SkeletonRecord>), 72 B/record'.
- CLIENT pull: crates/sigil-top/src/block_sync/fetch.rs:491 pull_snapshot;
  :511 BackfillReq{headers_only:true,codec}; :543 stream 'S' commit-as-you-stream.
  mod.rs:687-706 bulk-pull verified skeleton prefix; codec=1 crawl fallback.
- STORE: flux_db::skeleton::SkeletonStore (native) via block_sync/skel_flux.rs.
- e2e test + bench: fetch.rs:621 snapshot_pull_end_to_end_into_store; :681 blk/s bench.

## THE GAP (why it does not activate)
- Anchor TRUST: mod.rs:251 'NEVER trusts a network anchor without verify_signed_anchor (#417)';
  fetch.rs:158 verify_signed_anchor(&a, &producer_pk) needs a SIGNED anchor.
- Anchor PUBLISH: mod.rs:249 '_sigil-tip DNS TXT is a dead template (A #449)'. No live signer
  publishes the signed tip-anchor, so manual SIGIL_SNAPSHOT_ANCHOR=<height>:<hex32> has no
  signature -> verify fails -> pull never triggers -> falls back to full-block crawl.
- RUNTIME CONFIRMED: SIGIL_SNAPSHOT_ANCHOR=955107:26234c25... on the box did NOT activate
  (store stayed empty, produce-tip 0, chain-sync OFFLINE).

## TO BUILD (either half unblocks it)
1. Signed-anchor PUBLISHER on epsilon next to the producer: periodically SQIsign the finalized
   tip (height+block_hash) with the producer key and publish where fetch::fetch_verified_anchor_tip()
   reads (revive _sigil-tip DNS TXT, or a feed URL). 
   OR testnet shortcut: trusted manual-anchor path — when SIGIL_SNAPSHOT_ANCHOR is operator-set,
   trust height:hash directly (skip verify_signed_anchor), gated by e.g. SIGIL_TRUST_MANUAL_ANCHOR=1.
2. Confirm light client reaches a codec=2-serving peer over flux-p2p :9501 (box showed OFFLINE
   produce-tip 0 — verify the pull path engages once anchor trust passes).

## ACCEPTANCE
Fresh sigil-top + trusted anchor fast-syncs genesis->anchor via codec=2 skeleton pages,
measured >> full-block ~39/s (thousands/s, cf fetch.rs:681). produce-tip/net height reflect tip;
normal crawl anchor->tip after.
