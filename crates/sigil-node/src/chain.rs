//! In-memory chain tip tracker — Phase 0 substitute for `flux-db`-backed
//! persistence. Holds the current tip header + state, plus a vector of
//! historical blocks for replay.

use anyhow::{anyhow, Context, Result};

use sigil_header::{BlockHash, SigilBlockHeaderV0};
use sigil_state::{commit_state_transition, SigilState, StateRoots, StateTransition};

use std::collections::VecDeque;

use crate::block::Block;

/// Recent blocks kept in RAM. Older blocks are pruned from RAM and live on disk in
/// the append-only [`crate::chain_log::ChainLog`] (served from there on backfill).
/// This is what bounds producer memory so the chain can grow without OOM.
pub const WINDOW: usize = 8192;

/// One node's view of the chain. Phase 0: linear, single-producer. RAM holds only the
/// last `WINDOW` blocks (a sliding window) + the current state; `base_height` is the
/// height of the oldest in-RAM block, so `height()` stays correct after pruning.
pub struct ChainTip {
    state: SigilState,
    blocks: VecDeque<Block>,
    base_height: u64,
}

impl Default for ChainTip {
    fn default() -> Self {
        Self::new()
    }
}

impl ChainTip {
    /// Fresh chain — no genesis seeded yet.
    pub fn new() -> Self {
        Self { state: SigilState::new(), blocks: VecDeque::new(), base_height: 0 }
    }

    /// v0.36.1: reconstruct a ChainTip from snapshot parts — the accumulated
    /// state at the tip, the recent-block RAM window, and the window's base
    /// height. These three fields ARE the whole ChainTip, so a restore is
    /// bitwise-identical to having replayed every block up to the snapshot
    /// height; applying blocks `snapshot_height+1..` afterwards produces the
    /// exact same state/roots as a full replay. Callers (snapshot boot) MUST
    /// verify integrity first (BLAKE3 checksum + tip-hash-vs-chain.log check
    /// in `crate::snapshot::load_state` / `run_start`) — this constructor
    /// trusts its inputs.
    pub fn from_parts(state: SigilState, blocks: VecDeque<Block>, base_height: u64) -> Self {
        Self { state, blocks, base_height }
    }

    /// v0.36.1: borrow the three snapshot parts (state, RAM window, window
    /// base) for state-snapshot capture. Read-only — the caller clones what
    /// it persists.
    pub fn snapshot_parts(&self) -> (&SigilState, &VecDeque<Block>, u64) {
        (&self.state, &self.blocks, self.base_height)
    }

    /// Read-only snapshot of the four state roots at the current tip.
    pub fn roots(&self) -> StateRoots {
        self.state.roots()
    }

    /// Current chain height. `0` when no blocks have been applied.
    pub fn height(&self) -> u64 {
        if self.blocks.is_empty() { 0 } else { self.base_height + self.blocks.len() as u64 }
    }

    /// Block at `height` IF it is still in the in-RAM window; `None` for pruned
    /// (older) heights, which the caller fetches from [`crate::chain_log::ChainLog`].
    pub fn get(&self, height: u64) -> Option<&Block> {
        if height < self.base_height { return None; }
        self.blocks.get((height - self.base_height) as usize)
    }

    /// Lowest height currently held in RAM (older blocks are on disk).
    pub fn window_base(&self) -> u64 { self.base_height }

    /// Parent hash to set on the next block. All-zero before genesis is
    /// applied (genesis itself uses this as `parent_hash`).
    pub fn parent_hash(&self) -> BlockHash {
        self.blocks.back().map(|b| b.hash()).unwrap_or([0u8; 32])
    }

    /// Hand-back of the tip header for callers building auto-update
    /// activation windows etc.
    pub fn tip_header(&self) -> Option<&SigilBlockHeaderV0> {
        self.blocks.back().map(|b| &b.header)
    }

    /// Snapshot of the current state, for use by block builders that need to
    /// dry-run a transition before producing the block whose header will
    /// commit those roots. Cloning is fine in P0 (BTreeMap-backed state); P3
    /// will swap this for a read-locked SMT view.
    pub fn state_snapshot(&self) -> SigilState {
        self.state.clone()
    }

    /// Apply a block. Runs header precheck, applies the state transition
    /// through the chokepoint, verifies the resulting roots match the
    /// header's declared roots, and appends on success.
    ///
    /// Phase 0 omits crypto verification (SQIsign nonce, producer sig, VDF,
    /// STARK). Those land in P1+ when the relevant crates port.
    pub fn apply(&mut self, block: Block) -> Result<()> {
        block.header.precheck().with_context(|| "header precheck failed")?;

        let expected_height = self.height();
        if block.header.height != expected_height {
            return Err(anyhow!(
                "block height mismatch: chain expects {}, block claims {}",
                expected_height, block.header.height
            ));
        }
        if block.header.parent_hash != self.parent_hash() {
            return Err(anyhow!(
                "parent_hash mismatch: chain tip is {}, block parent is {}",
                hex_short(&self.parent_hash()),
                hex_short(&block.header.parent_hash),
            ));
        }
        if block.transition.at_height != expected_height {
            return Err(anyhow!(
                "transition height mismatch with header: header={}, transition={}",
                expected_height, block.transition.at_height
            ));
        }

        let computed = commit_state_transition(&mut self.state, &block.transition, expected_height)
            .map_err(|e| anyhow!("state commit failed: {}", e))?;

        if !block.check_roots_match(&computed) {
            return Err(anyhow!(
                "STATE DIVERGENCE at height {} — local roots != header roots", expected_height
            ));
        }

        self.blocks.push_back(block);
        while self.blocks.len() > WINDOW {
            self.blocks.pop_front();
            self.base_height += 1;
        }
        Ok(())
    }
}

fn hex_short(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    for byte in &b[..4] {
        s.push_str(&format!("{:02x}", byte));
    }
    s.push_str("…");
    s
}

#[cfg(test)]
mod window_accounting_tests {
    //! Pure window-accounting for the RAM sliding window (Tier 3 — chain.rs had
    //! zero tests). `get()`/`height()`/`window_base()` must stay correct after
    //! old blocks are pruned to disk; `from_parts`/`snapshot_parts` is the
    //! snapshot-boot equivalence contract. Uses opaque `__test_chain` blocks —
    //! `get()` indexes by window position, it never re-validates the block.
    use super::ChainTip;
    use crate::block::__test_chain;
    use sigil_state::SigilState;
    use std::collections::VecDeque;

    fn tip_with(n: u64, base: u64) -> ChainTip {
        let blocks: VecDeque<_> = __test_chain(n).into_iter().collect();
        ChainTip::from_parts(SigilState::new(), blocks, base)
    }

    #[test]
    fn height_is_base_plus_len_and_get_is_base_relative() {
        let t = tip_with(10, 100); // window holds 10 blocks based at height 100
        assert_eq!(t.height(), 110, "height = base + window len");
        assert_eq!(t.window_base(), 100);
        // pruned (below base) and beyond-tip heights are absent from RAM…
        assert!(t.get(99).is_none(), "pruned height is not in RAM");
        assert!(t.get(110).is_none(), "beyond-tip height is absent");
        // …everything inside [base, base+len) resolves.
        assert!(t.get(100).is_some(), "window base resolves");
        assert!(t.get(105).is_some());
        assert!(t.get(109).is_some(), "window tip resolves");
    }

    #[test]
    fn empty_window_is_height_zero_regardless_of_base() {
        let t = ChainTip::from_parts(SigilState::new(), VecDeque::new(), 50);
        assert_eq!(t.height(), 0, "empty window reports height 0 even with a nonzero base");
        assert!(t.get(0).is_none());
        assert!(t.get(50).is_none());
    }

    #[test]
    fn get_boundaries_at_zero_base() {
        let t = tip_with(5, 0);
        assert_eq!(t.height(), 5);
        for h in 0..5 {
            assert!(t.get(h).is_some(), "height {h} in window");
        }
        assert!(t.get(5).is_none(), "one past the tip is absent");
    }

    #[test]
    fn snapshot_parts_roundtrips_through_from_parts() {
        let t = tip_with(10, 100);
        let (_state, blocks, base) = t.snapshot_parts();
        assert_eq!(base, 100);
        assert_eq!(blocks.len(), 10);
        // Rebuild from the captured parts → identical window geometry, which is
        // the snapshot-boot equivalence guarantee (restore == replay-to-tip).
        let rebuilt = ChainTip::from_parts(SigilState::new(), blocks.clone(), base);
        assert_eq!(rebuilt.height(), t.height());
        assert_eq!(rebuilt.window_base(), t.window_base());
    }
}
