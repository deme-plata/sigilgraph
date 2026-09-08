//! follower_reorg — durable recovery from a producer-restart fork, on a follower.
//!
//! ## Why this exists (verified live 2026-09-08)
//!
//! A producer restart rewinds the node to its last state snapshot
//! (`SIGIL_SNAPSHOT_EVERY`, default 100k blocks) and re-mints DIFFERENT blocks
//! forward. A follower that had already applied the pre-restart blocks is now on
//! an orphaned fork. [`crate::chain::ChainTip::apply`] is strictly forward-only —
//! the next block's `parent_hash` must equal the current tip hash — so the
//! follower CANNOT climb back onto the canonical chain. It re-requests the same
//! range forever, applies nothing, and because an n=2 finality certificate needs
//! BOTH validators at the tip, ALL finality freezes with no operator signal.
//! Measured: happysrv asked Epsilon for `[5128373..=5128885]` 385+ times, applied
//! zero, `parent_hash mismatch: chain tip is b987796a…, block parent is 06effb36…`.
//!
//! ## The fix
//!
//! Keep a small ring of state checkpoints as the chain advances. On a persistent
//! parent-hash mismatch (the fork signal), roll the [`ChainTip`] back to the
//! newest checkpoint BELOW the fork and let the ordinary catch-up path pull the
//! canonical suffix forward from there. If that checkpoint is itself on the
//! forked branch (a deeper producer rewind), the next persistent mismatch rolls
//! back one more checkpoint — the rollback SELF-CORRECTS without any extra peer
//! round-trip. When the ring is exhausted (the fork is deeper than the ring
//! spans — necessarily deeper than the in-RAM block window too, so local history
//! can't help anyway), the follower is out of local history and must re-sync from
//! the peer; we log loudly and leave it, which is no worse than the pre-fix wedge
//! but finally visible.
//!
//! ## Cost & safety
//!
//! Each checkpoint stores the tip block plus the rmp-serialized [`SigilState`]
//! (~20 MB on the live chain — the same codec the on-disk snapshot uses), so the
//! ring's memory is bounded by `cap`. Capture is distance-triggered (every
//! `cadence` blocks of real advance), so it is robust to being polled on a coarse
//! cadence (the 5 s heartbeat). Gated behind `SIGIL_FOLLOWER_REORG=1`; unset ⇒
//! [`ReorgRing::from_env`] returns `None` and this module is entirely inert (zero
//! behavior change on the producer or any node that does not opt in).
//!
//! ## Not handled here (known follow-up)
//!
//! The append-only [`crate::chain_log::ChainLog`] still holds the forked suffix
//! after a live reorg. A reboot before a fresh snapshot passes the reorg point
//! could replay it; enabling follower snapshotting past the reorg tip closes that
//! window. This module fixes the LIVE freeze (the thing that froze finality); the
//! log-rewrite is tracked separately.

use std::collections::VecDeque;

use sigil_state::SigilState;

use crate::block::Block;
use crate::chain::ChainTip;

/// Consecutive fork hits before a rollback fires. A single mismatch can be a
/// transient race (a block arriving before its parent); a handful in a row is a
/// real fork the forward-only apply can never resolve.
const FORK_HITS_BEFORE_REORG: u32 = 3;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default)
}

/// One rollback point: the state as of `tip_height`, plus that tip block (so a
/// restore can rebuild a valid one-block window whose `parent_hash()` lets the
/// next canonical block apply).
pub struct Checkpoint {
    pub tip_height: u64,
    pub tip_block: Block,
    /// `rmp_serde::to_vec(&SigilState)` — the accumulated state AFTER applying
    /// the block at `tip_height`.
    pub state_rmp: Vec<u8>,
}

/// A bounded ring of recent state checkpoints + the fork-recovery state machine.
pub struct ReorgRing {
    cadence: u64,
    cap: usize,
    /// Tip height of the last capture (0 = none yet). Capture triggers on
    /// distance from this, not exact modulo, so a coarse poll never misses.
    last_capture: u64,
    /// Ascending by `tip_height`.
    ckpts: VecDeque<Checkpoint>,
    /// After a rollback, the height we rolled back to. A repeat fork must pick a
    /// STRICTLY older checkpoint than this, so successive forks walk backward
    /// instead of restoring the same forked point. Cleared once capture advances
    /// past it (we've resynced onto what we believe is canonical).
    last_restore_ceiling: Option<u64>,
    fork_hits: u32,
}

impl ReorgRing {
    /// `Some` iff `SIGIL_FOLLOWER_REORG=1`. Cadence/depth are env-tunable;
    /// defaults span 8192 blocks (= the RAM block window `WINDOW`), the deepest
    /// fork local history could ever resolve anyway.
    pub fn from_env() -> Option<Self> {
        if std::env::var("SIGIL_FOLLOWER_REORG").ok().as_deref() != Some("1") {
            return None;
        }
        let cadence = env_u64("SIGIL_REORG_CADENCE", 1024).max(1);
        let cap = env_u64("SIGIL_REORG_DEPTH", 8).clamp(2, 64) as usize;
        eprintln!(
            "🧊 follower reorg ARMED — checkpoint every {} blocks, keep {} (spans ~{} blocks)",
            cadence,
            cap,
            cadence.saturating_mul(cap as u64)
        );
        Some(Self::with_params(cadence, cap))
    }

    /// Construct a ring with explicit parameters, bypassing the env gate. Used
    /// by the chronos integration test, and by any caller that wants to arm
    /// reorg programmatically. `cap` is clamped to `[2, 64]`, `cadence` to `>= 1`.
    pub fn with_params(cadence: u64, cap: usize) -> Self {
        Self {
            cadence: cadence.max(1),
            cap: cap.clamp(2, 64),
            last_capture: 0,
            ckpts: VecDeque::new(),
            last_restore_ceiling: None,
            fork_hits: 0,
        }
    }

    /// Blocks the ring spans (informational, for logs).
    pub fn span(&self) -> u64 {
        self.cadence.saturating_mul(self.cap as u64)
    }

    /// Heights currently checkpointed (oldest→newest). Test/introspection.
    pub fn heights(&self) -> Vec<u64> {
        self.ckpts.iter().map(|c| c.tip_height).collect()
    }

    /// Capture the current tip state IF we have advanced `>= cadence` since the
    /// last capture. Cheap no-op otherwise; safe to call every loop tick.
    pub fn maybe_capture(&mut self, chain: &ChainTip) {
        let h = chain.height();
        if h == 0 {
            return;
        }
        let tip_height = h - 1;
        if self.last_capture != 0 && tip_height < self.last_capture.saturating_add(self.cadence) {
            return;
        }
        let Some(tip_block) = chain.tip_block().cloned() else {
            return;
        };
        let state_rmp = match rmp_serde::to_vec(chain.state()) {
            Ok(b) => b,
            Err(_) => return,
        };
        self.push(Checkpoint { tip_height, tip_block, state_rmp });
        self.last_capture = tip_height;
        // Advancing past the last rollback ceiling means catch-up succeeded onto
        // (what we believe is) the canonical chain — allow future forks to roll
        // back from the newest checkpoint again.
        if let Some(c) = self.last_restore_ceiling {
            if tip_height > c {
                self.last_restore_ceiling = None;
            }
        }
    }

    fn push(&mut self, ck: Checkpoint) {
        self.ckpts.push_back(ck);
        while self.ckpts.len() > self.cap {
            self.ckpts.pop_front();
        }
    }

    /// Record a parent-hash-mismatch fork hit; returns `true` once it has
    /// persisted enough consecutive ticks to warrant a rollback.
    pub fn note_fork_hit(&mut self) -> bool {
        self.fork_hits = self.fork_hits.saturating_add(1);
        self.fork_hits >= FORK_HITS_BEFORE_REORG
    }

    /// Reset the consecutive-fork counter (call when a block applies cleanly).
    pub fn clear_fork_hits(&mut self) {
        if self.fork_hits != 0 {
            self.fork_hits = 0;
        }
    }

    /// Roll `chain` back to the newest checkpoint strictly below the current tip
    /// (and below any prior rollback ceiling, so repeated forks walk backward).
    /// Returns the height rolled back to, or `None` when the ring holds nothing
    /// old enough — the caller must then fall back to a peer re-sync.
    pub fn reorg(&mut self, chain: &mut ChainTip) -> Option<u64> {
        let current_tip = chain.height().saturating_sub(1);
        let ceiling = self
            .last_restore_ceiling
            .map(|c| c.min(current_tip))
            .unwrap_or(current_tip);
        let idx = self.ckpts.iter().rposition(|c| c.tip_height < ceiling)?;

        // Clone what we need out of the ring before mutating it.
        let state: SigilState = rmp_serde::from_slice(&self.ckpts[idx].state_rmp).ok()?;
        let tip_height = self.ckpts[idx].tip_height;
        let tip_block = self.ckpts[idx].tip_block.clone();

        // Rebuild the chain from a minimal one-block window at the checkpoint.
        // `height() == tip_height + 1`, `parent_hash() == tip_block.hash()`, so
        // the next canonical block (tip_height + 1) applies cleanly.
        *chain = ChainTip::from_parts(state, VecDeque::from([tip_block]), tip_height);

        // Drop checkpoints newer than the restore point — they were on the
        // forked suffix we just abandoned.
        while self.ckpts.back().map(|c| c.tip_height > tip_height).unwrap_or(false) {
            self.ckpts.pop_back();
        }
        self.last_restore_ceiling = Some(tip_height);
        self.last_capture = tip_height;
        self.fork_hits = 0;
        Some(tip_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::__test_chain;
    use std::collections::VecDeque as VD;

    fn ring(cadence: u64, cap: usize) -> ReorgRing {
        ReorgRing {
            cadence,
            cap,
            last_capture: 0,
            ckpts: VecDeque::new(),
            last_restore_ceiling: None,
            fork_hits: 0,
        }
    }

    /// A checkpoint carrying block `h` as its tip and an empty state (opaque —
    /// these tests exercise ring bookkeeping + window geometry, not state).
    /// `__test_chain(n)` yields heights `1..=n`, so `__test_chain(h).last()` is
    /// the block at height `h` (h ≥ 1 in every caller below).
    fn ckpt_at(h: u64) -> Checkpoint {
        let blk = __test_chain(h).into_iter().last().unwrap();
        Checkpoint {
            tip_height: h,
            tip_block: blk,
            state_rmp: rmp_serde::to_vec(&SigilState::new()).unwrap(),
        }
    }

    #[test]
    fn note_fork_hit_fires_only_after_threshold() {
        let mut r = ring(1024, 8);
        assert!(!r.note_fork_hit());
        assert!(!r.note_fork_hit());
        assert!(r.note_fork_hit(), "third consecutive hit triggers");
        r.clear_fork_hits();
        assert!(!r.note_fork_hit(), "cleared → counting restarts");
    }

    #[test]
    fn ring_keeps_only_cap_newest() {
        let mut r = ring(1, 3);
        for h in 1..=10 {
            r.push(ckpt_at(h));
        }
        assert_eq!(r.heights(), vec![8, 9, 10], "oldest evicted, cap honored");
    }

    #[test]
    fn reorg_rolls_back_to_newest_below_tip_then_walks_older_on_repeat() {
        // Checkpoints at 100, 200, 300; pretend the chain tip is 350.
        let mut r = ring(100, 8);
        for h in [100u64, 200, 300] {
            r.push(ckpt_at(h));
        }
        // Build a chain whose tip is 350 so `reorg` sees current_tip = 350.
        let blocks: VD<_> = __test_chain(351).into_iter().collect();
        let mut chain = ChainTip::from_parts(SigilState::new(), blocks, 0);
        assert_eq!(chain.height(), 351);

        // First fork: roll back to the newest checkpoint below 350 → 300.
        let h1 = r.reorg(&mut chain).expect("checkpoint below 350 exists");
        assert_eq!(h1, 300);
        assert_eq!(chain.height(), 301, "tip is now block 300");

        // A fork at the RESTORED tip means 300 was itself forked — the next
        // rollback must pick STRICTLY older (200), never 300 again.
        let h2 = r.reorg(&mut chain).expect("older checkpoint exists");
        assert_eq!(h2, 200);
        assert_eq!(chain.height(), 201);

        let h3 = r.reorg(&mut chain).expect("oldest checkpoint");
        assert_eq!(h3, 100);

        // Ring exhausted below 100 → None (caller falls back to peer re-sync).
        assert!(r.reorg(&mut chain).is_none(), "nothing older than 100");
    }

    #[test]
    fn capture_advancing_past_ceiling_clears_it() {
        let mut r = ring(10, 8);
        for h in [100u64, 110, 120] {
            r.push(ckpt_at(h));
        }
        // Chain tip at 129 ⇒ newest checkpoint STRICTLY below is 120.
        let blocks: VD<_> = __test_chain(130).into_iter().collect();
        let mut chain = ChainTip::from_parts(SigilState::new(), blocks, 0);
        let rolled = r.reorg(&mut chain).unwrap();
        assert_eq!(rolled, 120);
        assert_eq!(r.last_restore_ceiling, Some(120));
        // Simulate catch-up well past the ceiling, then a capture.
        let blocks2: VD<_> = __test_chain(200).into_iter().collect();
        let chain2 = ChainTip::from_parts(SigilState::new(), blocks2, 0);
        r.maybe_capture(&chain2);
        assert_eq!(r.last_restore_ceiling, None, "advanced past ceiling → cleared");
    }
}
