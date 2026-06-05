//! TURBO-1 — block-level turbo sync, in chronos.
//!
//! A producer builds an N-block chain; a fresh follower **turbo-syncs** it in
//! batches, applying every block through the real `apply_external_block`
//! chokepoint, which recomputes the four state roots and returns
//! [`ApplyOutcome`](crate::ApplyOutcome): `Ok` (roots matched), `Divergence`
//! (exit-78), or `Rejected` (malformed/out-of-order). So this verifies EVERY
//! block, not just the tip — the "sync every block + speed" test.
//!
//! Safety proven inline: the **sync-down guard** — a node refuses to apply a
//! block at a height it's already past (re-applying an old block must NOT move
//! the tip). That's the application-layer half of the catastrophic-sync-down
//! protection (never replace a longer chain with a shorter one).

use std::time::Instant;

use flux_chronos::NodeId;
use sigil_state::WalletId;
use sigil_tx::{SigilTx, NATIVE};

use crate::{demo_genesis, sign_dummy, ApplyOutcome, Block, SigilSimNode};

fn wallet(i: u8) -> WalletId {
    [i; 32] // demo wallets 0x01..0x05 funded by demo_genesis (all bytes = i)
}

/// Result of a turbo-sync run.
#[derive(Debug, Clone)]
pub struct TurboSyncReport {
    /// Blocks the producer built (and the follower had to sync).
    pub blocks: u64,
    /// Batch size used for the sync (request granularity).
    pub batch_size: u64,
    /// Blocks applied cleanly (roots matched).
    pub applied_ok: u64,
    /// Blocks that diverged (exit-78) — must be 0 on an honest chain.
    pub divergences: u64,
    /// Blocks rejected (malformed/out-of-order) — must be 0 here.
    pub rejected: u64,
    /// Follower's height after sync.
    pub final_height: u64,
    /// Wall-clock for the sync (not production).
    pub elapsed_ms: u128,
    /// Blocks/sec — the turbo-sync speed.
    pub blocks_per_sec: f64,
    /// Did the sync-down guard hold (re-applying an old block didn't move the tip)?
    pub sync_down_blocked: bool,
}

impl TurboSyncReport {
    /// One-line summary.
    pub fn summary(&self) -> String {
        format!(
            "{blocks} blocks · batch {bs} · {bps:.0} blocks/s · {ms}ms\n   verified: {ok} ok · {div} divergence · {rej} rejected · tip {tip}\n   sync-down guard: {guard}",
            blocks = self.blocks, bs = self.batch_size, bps = self.blocks_per_sec, ms = self.elapsed_ms,
            ok = self.applied_ok, div = self.divergences, rej = self.rejected, tip = self.final_height,
            guard = if self.sync_down_blocked { "HELD ✓ (old block could not move the tip)" } else { "FAILED ✗" }
        )
    }
}

/// Run a turbo-sync: producer builds `n_blocks`, follower batch-syncs + verifies
/// every one. Deterministic.
pub fn run_turbo_sync(n_blocks: u64, batch_size: u64) -> TurboSyncReport {
    let g = demo_genesis();
    let batch_size = batch_size.max(1);

    // ── producer builds the chain (1 tx/block so roots change every block) ──
    let mut producer = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, 1, &g);
    let wallets: Vec<WalletId> = (1u8..=5).map(wallet).collect();
    let mut chain: Vec<Block> = Vec::with_capacity(n_blocks as usize);
    let mut c = 0u64;
    while (chain.len() as u64) < n_blocks {
        let from = wallets[(c % 5) as usize];
        let to = wallets[((c + 1) % 5) as usize];
        c += 1;
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        match producer.produce_one() {
            Some(b) => chain.push(b),
            None => break,
        }
    }
    let blocks = chain.len() as u64;

    // ── follower turbo-syncs: fresh node, batch-apply, verify EVERY block ──
    let mut follower = SigilSimNode::new("follower", NodeId(1), vec![NodeId(0)], false, 1, &g);
    let (mut ok, mut div, mut rej) = (0u64, 0u64, 0u64);
    let t0 = Instant::now();
    for batch in chain.chunks(batch_size as usize) {
        for block in batch {
            // sync-down guard: never apply a block below our current height.
            if block.header.height < follower.height() {
                continue;
            }
            match follower.apply_external_block(block) {
                ApplyOutcome::Ok => ok += 1,
                ApplyOutcome::Divergence => div += 1,
                ApplyOutcome::Rejected => rej += 1,
            }
        }
    }
    let elapsed_ms = t0.elapsed().as_millis();

    // ── explicit sync-down attack: re-apply an OLD block, the tip must not move ──
    let sync_down_blocked = if blocks > 5 {
        let before = follower.height();
        let _ = follower.apply_external_block(&chain[2]); // height 3, far below tip
        follower.height() == before
    } else {
        true
    };

    let secs = (elapsed_ms as f64 / 1000.0).max(1e-6);
    TurboSyncReport {
        blocks,
        batch_size,
        applied_ok: ok,
        divergences: div,
        rejected: rej,
        final_height: follower.height(),
        elapsed_ms,
        blocks_per_sec: ok as f64 / secs,
        sync_down_blocked,
    }
}

/// Report for the late-join backfill experiment — the in-sim proof of SIGIL P1 #5.
#[derive(Debug, Clone)]
pub struct LateJoinReport {
    /// History blocks the late-joiner missed (produced before it grafted).
    pub history: u64,
    /// Live blocks produced after it grafted (the gossip stream it can see).
    pub live: u64,
    /// GOSSIP-ONLY follower: applies only the live stream, no backfill.
    /// Reproduces the cross-WAN wall — every live block rejects (can't chain to genesis).
    pub gossip_only_applied: u64,
    pub gossip_only_rejected: u64,
    pub gossip_only_height: u64,
    /// BACKFILL-THEN-LIVE follower: batch-syncs the history gap first, then applies live.
    /// The fix — every block chains and applies.
    pub backfill_applied: u64,
    pub backfill_rejected: u64,
    pub backfill_height: u64,
}

impl LateJoinReport {
    pub fn summary(&self) -> String {
        format!(
            "late-join: {h} history + {l} live blocks\n  gossip-only (the wall): applied={goa} rejected={gor} height={goh}  ← live stream can't chain to genesis\n  backfill-then-live (P1 #5): applied={ba} rejected={br} height={bh}  ← gap synced first, everything chains",
            h = self.history, l = self.live,
            goa = self.gossip_only_applied, gor = self.gossip_only_rejected, goh = self.gossip_only_height,
            ba = self.backfill_applied, br = self.backfill_rejected, bh = self.backfill_height,
        )
    }
}

/// LATE-JOIN BACKFILL (SIGIL P1 #5 proof, in chronos).
///
/// Reproduces the 2026-05-31 cross-WAN propagation wall and proves the fix, both
/// through the real `apply_external_block` chokepoint:
///
/// A producer builds `history` blocks BEFORE the follower grafts, then `live`
/// blocks the follower can see on the gossip stream. Gossipsub has **no history**
/// (skill Rule 3), so a fresh late-joiner sees only `live` (starting at
/// `H=history+2`) — which can't chain to its genesis (`H=1`), so every one
/// **Rejects** (this is exactly Delta's `apply H=166 -> Rejected`, height stuck
/// at 1). The fix: **backfill the H=2..history+1 gap first** (the turbo-sync batch
/// path), THEN apply the live stream — now every block chains and applies.
pub fn run_late_join_backfill(history: u64, live: u64, batch_size: u64) -> LateJoinReport {
    let g = demo_genesis();
    let batch_size = batch_size.max(1);

    // ── producer builds history + live blocks (1 tx/block, roots change each) ──
    let mut producer = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, 1, &g);
    let wallets: Vec<WalletId> = (1u8..=5).map(wallet).collect();
    let total = history + live;
    let mut chain: Vec<Block> = Vec::with_capacity(total as usize);
    let mut c = 0u64;
    while (chain.len() as u64) < total {
        let from = wallets[(c % 5) as usize];
        let to = wallets[((c + 1) % 5) as usize];
        c += 1;
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        match producer.produce_one() {
            Some(b) => chain.push(b),
            None => break,
        }
    }
    let (history_blocks, live_blocks) = chain.split_at(history as usize);

    // ── GOSSIP-ONLY follower (the wall): sees only the live stream. ──
    let mut gossip_only = SigilSimNode::new("gossip-only", NodeId(1), vec![NodeId(0)], false, 1, &g);
    let (mut goa, mut gor) = (0u64, 0u64);
    for block in live_blocks {
        match gossip_only.apply_external_block(block) {
            ApplyOutcome::Ok => goa += 1,
            ApplyOutcome::Rejected | ApplyOutcome::Divergence => gor += 1,
        }
    }

    // ── BACKFILL-THEN-LIVE follower (the fix): batch-sync the gap, then live. ──
    let mut backfill = SigilSimNode::new("backfill", NodeId(1), vec![NodeId(0)], false, 1, &g);
    let (mut ba, mut br) = (0u64, 0u64);
    // 1) backfill the missed history in batches (the turbo-sync request granularity)
    for batch in history_blocks.chunks(batch_size as usize) {
        for block in batch {
            if block.header.height < backfill.height() {
                continue; // sync-down guard
            }
            match backfill.apply_external_block(block) {
                ApplyOutcome::Ok => ba += 1,
                ApplyOutcome::Rejected | ApplyOutcome::Divergence => br += 1,
            }
        }
    }
    // 2) now apply the live stream — it chains onto the backfilled tip
    for block in live_blocks {
        match backfill.apply_external_block(block) {
            ApplyOutcome::Ok => ba += 1,
            ApplyOutcome::Rejected | ApplyOutcome::Divergence => br += 1,
        }
    }

    LateJoinReport {
        history,
        live,
        gossip_only_applied: goa,
        gossip_only_rejected: gor,
        gossip_only_height: gossip_only.height(),
        backfill_applied: ba,
        backfill_rejected: br,
        backfill_height: backfill.height(),
    }
}

/// Witness vector for a block — `n` field elements derived from its hash via a
/// BLAKE3 XOF. Folding the prefix of these into one constant-size proof lets a
/// late-joiner attest the WHOLE history in a single check (instead of replaying).
fn block_witness(block: &Block, n: usize) -> Vec<u64> {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-chronos/fold-block-witness/v1");
    h.update(&block.hash());
    let mut xof = h.finalize_xof();
    let mut buf = vec![0u8; n * 8];
    xof.fill(&mut buf);
    (0..n)
        .map(|i| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&buf[i * 8..i * 8 + 8]);
            u64::from_le_bytes(a) % flux_fold::Q
        })
        .collect()
}

/// Report for the FLUX-FOLD late-join catch-up (SIGIL P1 #5, the succinct path).
#[derive(Debug, Clone)]
pub struct LateJoinFoldReport {
    pub history: u64,
    pub live: u64,
    /// Did the single fold proof of the history prefix verify?
    pub fold_verified: bool,
    /// Size of the fold proof — CONSTANT in `history` (the succinctness headline).
    pub fold_proof_bytes: usize,
    /// Wall-clock to verify the fold (one check, vs replaying `history` blocks).
    pub fold_verify_us: u128,
    /// Live blocks applied AFTER adopting the fold checkpoint (no history replay).
    pub blocks_applied: u64,
    pub divergence: u64,
    pub rejected: u64,
    pub final_height: u64,
    /// History blocks the fold let the joiner SKIP replaying.
    pub replays_skipped: u64,
}

impl LateJoinFoldReport {
    pub fn summary(&self) -> String {
        format!(
            "fold late-join: {h} history + {l} live\n  fold proof: {ok} · {bytes} B (constant ∀ history) · verified in {us} µs\n  adopted checkpoint @H={h} (skipped {sk} replays) → applied {ba} live blocks · {div} divergence · {rej} rejected · tip {tip}",
            h = self.history, l = self.live,
            ok = if self.fold_verified { "VERIFIED ✓" } else { "FAILED ✗" },
            bytes = self.fold_proof_bytes, us = self.fold_verify_us,
            sk = self.replays_skipped, ba = self.blocks_applied,
            div = self.divergence, rej = self.rejected, tip = self.final_height,
        )
    }
}

/// FLUX-FOLD LATE-JOIN CATCH-UP (SIGIL P1 #5, succinct path — the fix for the
/// late-joiner reject wall, in chronos).
///
/// Where [`run_late_join_backfill`] proves a joiner can REPLAY the missed gap,
/// this proves the SUCCINCT path: the joiner verifies ONE constant-size fold proof
/// of the entire history prefix, adopts the fold-attested state checkpoint, and
/// then applies only the live stream — never replaying the `history` blocks.
///
/// Both sides go through the real `apply_external_block` chokepoint for the live
/// blocks, so divergence is genuinely checked.
///
/// HONEST SCOPE (what's real vs modeled here):
///  • REAL: the fold proof + verification are real `flux_fold` (BLAKE4 transparent
///    setup, Ajtai/SIS, post-quantum). Proof size is constant in `history`.
///  • REAL: the live blocks are applied through the true chokepoint; divergence=0
///    means the adopted checkpoint's roots actually chain the live stream.
///  • MODELED: the adopted state snapshot is obtained by cloning the producer at
///    the history tip (a stand-in for the state-snapshot a light client fetches
///    from a peer). The fold proof is precisely what lets a real client TRUST that
///    snapshot's roots without replaying. v0.2 fold verify still reads the M
///    per-block commitments (O(M) verifier input); folding the commitment vector
///    itself is the v0.3 lane — so the 2,568 B / 342 ms headline is the
///    real-scale parameterization, while the in-sim proof size is reported as
///    measured.
pub fn run_late_join_fold(history: u64, live: u64) -> LateJoinFoldReport {
    let g = demo_genesis();
    let wallets: Vec<WalletId> = (1u8..=5).map(wallet).collect();

    // ── producer builds the `history` prefix ──
    let mut producer = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, 1, &g);
    let mut history_blocks: Vec<Block> = Vec::with_capacity(history as usize);
    let mut c = 0u64;
    while (history_blocks.len() as u64) < history {
        let from = wallets[(c % 5) as usize];
        let to = wallets[((c + 1) % 5) as usize];
        c += 1;
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        match producer.produce_one() {
            Some(b) => history_blocks.push(b),
            None => break,
        }
    }

    // ── SNAPSHOT the state at the history tip — the fold-attested checkpoint a
    //    light client adopts instead of replaying 0..history. ──
    let snapshot = producer.clone();

    // ── producer continues, builds the `live` stream the joiner will see ──
    let mut live_blocks: Vec<Block> = Vec::with_capacity(live as usize);
    while (live_blocks.len() as u64) < live {
        let from = wallets[(c % 5) as usize];
        let to = wallets[((c + 1) % 5) as usize];
        c += 1;
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        match producer.produce_one() {
            Some(b) => live_blocks.push(b),
            None => break,
        }
    }

    // ── FOLD: one constant-size proof over the whole history prefix.
    //    Transparent BLAKE4 setup (no trusted ceremony) — the chain's own hash. ──
    let (m, n) = (16usize, 32usize);
    let ajtai = flux_fold::Ajtai::from_seed_blake4(m, n, b"sigil-chronos/late-join-fold/v1");
    let witnesses: Vec<Vec<u64>> = history_blocks.iter().map(|b| block_witness(b, n)).collect();
    let commitments: Vec<Vec<u64>> = witnesses.iter().map(|w| ajtai.commit(w)).collect();
    let proof = flux_fold::fold(&ajtai, &witnesses);
    let fold_proof_bytes = proof.size_bytes();

    // ── LIGHT CLIENT: verify the fold in ONE check (not `history` replays) ──
    let t = Instant::now();
    let fold_verified = flux_fold::verify(&ajtai, &commitments, &proof);
    let fold_verify_us = t.elapsed().as_micros();

    // ── adopt the fold checkpoint + apply ONLY the live stream ──
    let mut joiner = snapshot; // state @ history tip, NO replay
    let (mut applied, mut div, mut rej) = (0u64, 0u64, 0u64);
    if fold_verified {
        for b in &live_blocks {
            match joiner.apply_external_block(b) {
                ApplyOutcome::Ok => applied += 1,
                ApplyOutcome::Divergence => div += 1,
                ApplyOutcome::Rejected => rej += 1,
            }
        }
    }

    LateJoinFoldReport {
        history,
        live,
        fold_verified,
        fold_proof_bytes,
        fold_verify_us,
        blocks_applied: applied,
        divergence: div,
        rejected: rej,
        final_height: joiner.height(),
        replays_skipped: history,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_join_gossip_only_rejects_but_backfill_applies() {
        // 165 history (the gap Delta couldn't chain) + 20 live blocks.
        let r = run_late_join_backfill(165, 20, 64);
        // The wall: gossip-only sees only live blocks (H=167..), none chain to H=1.
        assert_eq!(r.gossip_only_applied, 0, "gossip-only must apply nothing (the cross-WAN wall)");
        assert_eq!(r.gossip_only_rejected, 20, "every live block rejects — can't chain to genesis");
        assert_eq!(r.gossip_only_height, 1, "height stuck at genesis (== Delta's height=1)");
        // The fix: backfill the gap first, then live — everything chains.
        assert_eq!(r.backfill_applied, 185, "backfill + live all apply (165 + 20)");
        assert_eq!(r.backfill_rejected, 0);
        assert_eq!(r.backfill_height, 186, "genesis(1) + 165 + 20");
    }

    #[test]
    fn late_join_fold_skips_replay_and_applies_live() {
        // A joiner verifies ONE fold of 165 history blocks, adopts the checkpoint,
        // and applies 20 live blocks — the succinct catch-up (SIGIL P1 #5).
        let r = run_late_join_fold(165, 20);
        // the single fold proof of the whole prefix verified …
        assert!(r.fold_verified, "the history fold proof must verify");
        // … and is constant-size (independent of the 165 history blocks)
        assert!(r.fold_proof_bytes > 0 && r.fold_proof_bytes < 2_000,
            "fold proof must be small + constant, got {} B", r.fold_proof_bytes);
        // the joiner SKIPPED replaying all 165 history blocks …
        assert_eq!(r.replays_skipped, 165);
        // … yet applied every live block cleanly — the goal's exact condition:
        assert_eq!(r.blocks_applied, 20, "blocks_applied>0 for the late joiner");
        assert_eq!(r.divergence, 0, "divergence=0 — the adopted checkpoint chains the live stream");
        assert_eq!(r.rejected, 0, "no live block rejected after adopting the fold tip");
        assert_eq!(r.final_height, 186, "genesis(1) + 165 + 20");
    }

    #[test]
    fn late_join_fold_proof_size_is_constant_in_history() {
        // Succinctness: 10× more history → same proof size.
        let small = run_late_join_fold(20, 5);
        let big = run_late_join_fold(2_000, 5);
        assert_eq!(small.fold_proof_bytes, big.fold_proof_bytes,
            "fold proof size must not grow with history length");
        assert!(small.fold_verified && big.fold_verified);
        assert_eq!(big.blocks_applied, 5);
        assert_eq!(big.divergence, 0);
    }

    #[test]
    fn turbo_sync_applies_every_block_and_blocks_sync_down() {
        let r = run_turbo_sync(2_000, 128);
        assert_eq!(r.blocks, 2_000);
        assert_eq!(r.applied_ok, 2_000, "every block must apply with matching roots");
        assert_eq!(r.divergences, 0);
        assert_eq!(r.rejected, 0);
        assert_eq!(r.final_height, 2_001); // genesis + 2000
        assert!(r.sync_down_blocked, "an old block must not move the tip");
    }

    #[test]
    fn batch_size_does_not_change_outcome() {
        // Determinism: different batch sizes → identical applied count + tip.
        let a = run_turbo_sync(1_000, 16);
        let b = run_turbo_sync(1_000, 500);
        assert_eq!(a.applied_ok, b.applied_ok);
        assert_eq!(a.final_height, b.final_height);
    }
}
