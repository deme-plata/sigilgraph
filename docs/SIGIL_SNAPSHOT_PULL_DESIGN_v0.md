# SIGIL snapshot-pull integration design (LANE-A) — v0

> Author: rocky-sync-A. Designed with two Claude Code subagents + DeepSeek adversarial
> review, 2026-06-19. Status: **DESIGN READY — wiring gated on lead's go/no-go AND the
> two CRITICAL soundness fixes below.** Companion to docs/SIGIL_SKELETON_CODEC2_v0.md.

## Goal

Make block sync *be rsync*: bulk-pull a verified skeleton snapshot of the historical
prefix `[base, anchor]` (72 B/record, ~7.2 MB for 100k blocks) in a few large streamed
responses, verify stream-as-you-go, then run per-block libp2p only for the live frontier.
Measured: 100k skeletons = 7.2 MB → <0.1 s at the operator's proven 100 MB/s rsync rate
(~1.4M blk/s ceiling) + ~4 ms BLAKE3 verify. The 144 blk/s live = 1.18% of the pipe.

## fetch.rs surface (LANE-A owns)

```rust
/// Pull + structurally-verify a snapshot of [base, anchor]. The `send` closure wraps
/// net.send_request so fetch.rs needn't name libp2p::PeerId (sigil-top has no direct
/// libp2p dep). Returns the crypto-facts for LANE-B; Err → caller falls back to codec=1.
pub(super) async fn pull_snapshot<P, F, Fut>(peers: &[P], send: F)
    -> Result<SnapshotVerified, SnapshotError>
where P: Clone, F: Fn(P, Vec<u8>) -> Fut, Fut: Future<Output = Result<Vec<u8>, String>>;
```
Driver already landed: `SnapshotVerifier` / `SkeletonRecord`(72 B) / `SnapshotHeader` /
`SnapshotTrailer` / `SnapshotVerified` (fetch.rs, commit f6d2040).

## Wire (additive — old nodes downgrade structurally to 'H'/'Z')

`BackfillReq` gains two `#[serde(default)]` flags; `codec=2` = skeleton. Response selected
by first byte, reusing the existing `match data[0]` dispatch:

| tag | body | request |
|---|---|---|
| `'P'` | `bincode(SnapshotHeader)` | `codec=2, snapshot_header=true` |
| `'S'` | `bincode(Vec<SkeletonRecord>)` (≤ PAGE recs) | `codec=2` over `[from,to]` |
| `'F'` | `bincode(FoldCheckpoint) ‖ bincode(SnapshotTrailer)` | `codec=2, fold_range=true` |

Sequence: `'P'` (discover + header, also the capability probe) → stream `'S'` pages →
`'F'` (fold + root + sig). `PAGE = 50_000` recs = 3.6 MB/response (`SIGIL_SNAP_PAGE`);
~134 requests for a 6.7M prefix vs ~1640 at the 4096 live-CHUNK.

## launch() integration (lead owns mod.rs; this is the call shape)

One-shot pre-frontier block, gated: far-behind (`peer_best - synced_to >= SNAPSHOT_MIN_GAP`)
+ cold (`synced_to <= sync_base`) + DNS anchor resolved + peers present + `SIGIL_SNAPSHOT!=0`.
`Ok(verified)` → hand-off; `Err` → bench peer + fall through; **no peer serves it → codec=1
crawl runs unchanged (zero regression)**. After hand-off, `store.synced_to() == anchor` so
the existing frontier refill (`reads store.synced_to() live`) auto-resumes from the anchor —
no change to grok's window loop; the snapshot await never touches `done_tx`/`assigned`/`inflight`.

**Hand-off:** (a) commit skeletons via `store.put_block_raw(rec.height, &hex::encode(rec.block_hash))`
(NOT `put_blocks_batch` — skeletons aren't full headers); (b) `verify::fast_forward_to_anchored_checkpoint(store, anchor_height, &anchor_hash, frontier_window)` pins the watermark (it re-auths the anchor block by hash + full-verifies the window below it); (c) resume frontier.

## OOM latch

Stream-verify-then-drop: each `'S'` page → `verifier.push()` per record → page `Vec` dropped.
Verifier holds only `hasher + prev_block_hash(32B) + counters`. `adaptive_inflight(.., hi=2)`
⇒ ≤ 2 × 3.6 MB = **7.2 MB resident regardless of range size**. Per-page count cap mirrors the
existing 64 MB zstd-bomb guard.

## ✅ RESOLVED before activation (DeepSeek + B #416)

1. **State-root binding → CLOSED by removal.** The 4 state roots are DROPPED from the wire
   (SkeletonRecord is now 72 B: height + block_hash + parent_hash). They couldn't be made
   sound on the prefix — the fold is PoK over peer-supplied commitments (flat order-
   independent sum), so binding roots into the witness still admits fake interior pairs, and
   roots don't chain like `parent_hash`. CONTRACT: trusted state roots come ONLY from the
   frontier's real 8 KB headers or the DNS anchor; consumers needing prefix roots do an
   on-demand full-header fetch. `header_witness` stays `f(block_hash)`.
2. **Anchor freshness → CLOSED.** The DNS SQIsign anchor signs the TUPLE
   `(block_hash ‖ 4 roots ‖ height ‖ epoch)` with a strictly-monotonic NON-WRAPPING epoch;
   verifier rejects `epoch ≤ last-accepted` AND `age > MAX_ANCHOR_AGE`. Producer =
   sigil-dns-anchor / `dns_anchor_tip()`; verify = LANE-B (B flagged the lead).
3. **[HIGH] Resource bounds.** Cap `header.count` vs `(anchor-base+1)` and a hard
   `SNAPSHOT_MAX_RECORDS`; cap per-page records at PAGE; cap `FoldCheckpoint.commitments.len`.
   (`SkeletonRecord` uses fixed `[u8;32]` so per-field overflow is N/A under bincode.)
4. **[HIGH] Liveness.** Parallel-probe `'P'` across ≥3 peers, aggressive per-stage timeouts
   ('S' 5 s, 'F' 10 s), retry ≤3 peers, then IMMEDIATE codec=1 fallback (no exp backoff).

## Open store change

`put_block_raw` persists only `(height, hash)`, not the 4 roots on `SkeletonRecord`. If any
verified consumer needs the roots from disk, add `put_skeleton_batch` (store owner) — but see
MUST-FIX #1: the roots aren't verified anyway, so persisting them as trusted is itself gated
on #1's resolution.

## Impl notes

- The `pull_snapshot` sketch uses `futures::FuturesUnordered`; confirm `futures` is a sigil-top
  dep at wiring time (else hand-roll with tokio mpsc like the existing window).
- `'F'` body = two concatenated bincode values; decode positionally with one cursor (LANE-B
  confirms the exact split). Verifier only needs the `SnapshotTrailer`.
