//! What the gauge is allowed to look at.
//!
//! The gauge takes a *sample* of live counters, not a chain handle. Keeping the
//! input a plain struct is what makes this crate testable, deterministic, and
//! reusable across Quillon Graph, SIGIL, and a simulator — none of which share
//! a block type.
//!
//! Two samples one window apart give you the deltas the gauge needs. The block
//! window gives you the DAG facts (heights, producers, merge-parent counts)
//! that the commitment and anticone modules need.

use serde::{Deserialize, Serialize};

use crate::constants::{KAPPA, N_TOTAL_FALLBACK};
use crate::provenance::{Provenance, Tracked};

/// A single sample of monotone node counters, taken at one instant.
///
/// All fields are cumulative counters except `peer_count`, which is a level.
/// Deltas are taken between two consecutive samples; that is why the counters
/// must be monotone (saturating subtraction guards against a restart resetting
/// them, which shows up as a zero delta rather than a huge negative one).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSample {
    /// Mining solutions submitted to this node, cumulative.
    pub mining_submitted: u64,
    /// Mining solutions accepted by this node, cumulative.
    pub mining_accepted: u64,
    /// P2P bytes received, cumulative.
    pub p2p_bytes_in: u64,
    /// P2P bytes sent, cumulative.
    pub p2p_bytes_out: u64,
    /// Currently connected peers (a level, not a counter).
    pub peer_count: u64,
    /// This node's chain height.
    pub local_height: u64,
    /// Best height this node has heard about from peers. Zero means "unknown",
    /// which is treated as unavailable rather than as "we are fully synced".
    pub network_height: u64,
}

/// One block as the gauge needs to see it. Deliberately minimal — no bodies, no
/// signatures, no chain types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockFact {
    pub height: u64,
    /// Producer identity (public key, miner id — whatever the chain uses).
    /// Used only for the diversity check; never interpreted.
    pub producer: [u8; 32],
    /// How many *extra* parents beyond the single selected parent this block
    /// merged. `0` means the block is a straight chain link. This is the raw
    /// input to the DAG-Knight anticone measurement.
    pub merge_parent_count: u32,
    /// The chain's own blue-score for this block, if it reports one.
    pub blue_score: Option<u64>,
    /// The chain's own blue/red classification, if it reports one.
    pub is_blue: Option<bool>,
}

/// A contiguous run of recent blocks, oldest first.
///
/// The window is the unit over which `f_irrev` (Eq. 23) is defined, and its
/// *length in blocks* determines whether `f_irrev` is answerable at all — see
/// [`BlockWindow::spans_reorg_depth`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockWindow {
    pub blocks: Vec<BlockFact>,
}

impl BlockWindow {
    pub fn new(blocks: Vec<BlockFact>) -> Self {
        let mut blocks = blocks;
        blocks.sort_by_key(|b| b.height);
        Self { blocks }
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn tip_height(&self) -> Option<u64> {
        self.blocks.last().map(|b| b.height)
    }

    pub fn base_height(&self) -> Option<u64> {
        self.blocks.first().map(|b| b.height)
    }

    /// Number of distinct producers in the window. A window with one producer
    /// is a single-operator chain regardless of how healthy every other metric
    /// looks — the gauge cannot see Byzantine behaviour it is not exposed to.
    pub fn distinct_producers(&self) -> usize {
        let mut seen: Vec<[u8; 32]> = Vec::new();
        for b in &self.blocks {
            if !seen.contains(&b.producer) {
                seen.push(b.producer);
            }
        }
        seen.len()
    }

    /// True when the window is long enough that `f_irrev` (Eq. 23) can be
    /// anything other than zero.
    ///
    /// This is the guard the reference implementation is missing. `f_irrev`
    /// counts blocks whose commitment depth exceeds `D_reorg`. The deepest
    /// block in a window of length `L` has depth at most `L - 1`. So if
    /// `L <= D_reorg` the answer is *structurally* zero — not because the chain
    /// is unsettled, but because we did not look far enough back. Reporting
    /// that zero as a measurement is the exact failure the paper warns about.
    pub fn spans_reorg_depth(&self, d_reorg: u64) -> bool {
        self.len() as u64 > d_reorg
    }

    /// Blocks that merged at least one extra parent — i.e. real DAG width.
    pub fn merging_blocks(&self) -> usize {
        self.blocks.iter().filter(|b| b.merge_parent_count > 0).count()
    }

    /// Sum of extra parents across the window.
    pub fn total_merge_parents(&self) -> u64 {
        self.blocks.iter().map(|b| b.merge_parent_count as u64).sum()
    }

    /// Blocks whose reported `is_blue` disagrees with a monotonically
    /// increasing `blue_score`. A chain that increments blue-score while
    /// flagging every block red is reporting inconsistently, and any gauge
    /// reading derived from its colouring is suspect.
    pub fn colouring_inconsistencies(&self) -> usize {
        let mut bad = 0usize;
        let mut prev_score: Option<u64> = None;
        for b in &self.blocks {
            if let (Some(score), Some(is_blue)) = (b.blue_score, b.is_blue) {
                if let Some(p) = prev_score {
                    // Blue score advanced, so this block was counted as blue by
                    // the scoring rule, yet it is flagged red.
                    if score > p && !is_blue {
                        bad += 1;
                    }
                }
                prev_score = Some(score);
            }
        }
        bad
    }
}

/// How the network size `n_total` was obtained.
///
/// Paper limitation L11: `n_total` is hardcoded, and until a DHT crawl provides
/// it, `Omega_node` is "a model with one hardcoded input". This enum makes that
/// distinction load-bearing instead of a footnote.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "n")]
pub enum NetworkSize {
    /// Exact, from a registry or a permissioned validator set.
    Known(u64),
    /// From a real estimator — a DHT crawl, a peer-exchange census.
    Estimated(u64),
    /// A guess baked into the source. Poisons `Omega_node` down to
    /// [`Provenance::Placeholder`], which is the honest answer.
    Placeholder(u64),
}

impl Default for NetworkSize {
    fn default() -> Self {
        NetworkSize::Placeholder(N_TOTAL_FALLBACK)
    }
}

impl NetworkSize {
    pub fn value(self) -> u64 {
        match self {
            NetworkSize::Known(n) | NetworkSize::Estimated(n) | NetworkSize::Placeholder(n) => n,
        }
    }

    pub fn provenance(self) -> Provenance {
        match self {
            NetworkSize::Known(_) => Provenance::Measured,
            NetworkSize::Estimated(_) => Provenance::Derived,
            NetworkSize::Placeholder(_) => Provenance::Placeholder,
        }
    }

    pub fn tracked(self) -> Tracked<f64> {
        let v = self.value().max(1) as f64;
        match self {
            NetworkSize::Known(_) => Tracked::measured(v, "n_total from an authoritative set"),
            NetworkSize::Estimated(_) => {
                Tracked::derived(v, "n_total from a peer-discovery estimator")
            }
            NetworkSize::Placeholder(_) => {
                Tracked::placeholder(v, "n_total is hardcoded — no network-size estimator wired (paper L11)")
            }
        }
    }
}

/// Everything one gauge round needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observables {
    /// Counters at the start of the window.
    pub previous: CounterSample,
    /// Counters at the end of the window.
    pub current: CounterSample,
    /// Wall-clock length of the window in seconds. This is the `tau` of Eq. 10.
    pub window_secs: f64,
    /// Recent blocks, oldest first. May be empty; the commitment and anticone
    /// modules then report `Unavailable` rather than inventing a number.
    pub window: BlockWindow,
    /// Estimated total network size for `Omega_node` (Eq. 17).
    pub network_size: NetworkSize,
    /// DAG-Knight `kappa`. Protocol constant; kept configurable because a
    /// sibling chain may pick a different one.
    pub kappa: f64,
}

impl Default for Observables {
    fn default() -> Self {
        Self {
            previous: CounterSample::default(),
            current: CounterSample::default(),
            window_secs: crate::constants::DEFAULT_TAU_SECS,
            window: BlockWindow::default(),
            network_size: NetworkSize::default(),
            kappa: KAPPA,
        }
    }
}

impl Observables {
    /// Blocks added during the window, from the height counters.
    pub fn height_delta(&self) -> u64 {
        self.current
            .local_height
            .saturating_sub(self.previous.local_height)
    }

    /// Observed block rate in blocks per second, or `None` if the window has no
    /// duration.
    pub fn block_rate(&self) -> Option<f64> {
        if self.window_secs > 0.0 {
            Some(self.height_delta() as f64 / self.window_secs)
        } else {
            None
        }
    }

    pub fn submitted_delta(&self) -> u64 {
        self.current
            .mining_submitted
            .saturating_sub(self.previous.mining_submitted)
    }

    pub fn accepted_delta(&self) -> u64 {
        self.current
            .mining_accepted
            .saturating_sub(self.previous.mining_accepted)
    }

    pub fn bytes_in_delta(&self) -> u64 {
        self.current
            .p2p_bytes_in
            .saturating_sub(self.previous.p2p_bytes_in)
    }

    pub fn bytes_out_delta(&self) -> u64 {
        self.current
            .p2p_bytes_out
            .saturating_sub(self.previous.p2p_bytes_out)
    }

    /// True when nothing at all moved during the window. A gauge reading of
    /// zero from an idle node means "no evidence", not "perfect health", and
    /// this is how the gauge tells the two apart.
    pub fn is_quiescent(&self) -> bool {
        self.submitted_delta() == 0
            && self.bytes_in_delta() == 0
            && self.bytes_out_delta() == 0
            && self.height_delta() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(height: u64, producer: u8, merges: u32) -> BlockFact {
        BlockFact {
            height,
            producer: [producer; 32],
            merge_parent_count: merges,
            blue_score: Some(height),
            is_blue: Some(true),
        }
    }

    #[test]
    fn window_sorts_and_reports_bounds() {
        let w = BlockWindow::new(vec![blk(3, 1, 0), blk(1, 1, 0), blk(2, 1, 0)]);
        assert_eq!(w.base_height(), Some(1));
        assert_eq!(w.tip_height(), Some(3));
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn short_window_cannot_answer_f_irrev() {
        let w = BlockWindow::new((0..200).map(|h| blk(h, 1, 0)).collect());
        assert!(!w.spans_reorg_depth(360));
        let w = BlockWindow::new((0..400).map(|h| blk(h, 1, 0)).collect());
        assert!(w.spans_reorg_depth(360));
    }

    #[test]
    fn distinct_producers_counts_operators() {
        let w = BlockWindow::new(vec![blk(1, 7, 0), blk(2, 7, 0), blk(3, 9, 0)]);
        assert_eq!(w.distinct_producers(), 2);
    }

    #[test]
    fn straight_chain_has_no_merge_parents() {
        let w = BlockWindow::new(vec![blk(1, 1, 0), blk(2, 1, 0), blk(3, 1, 0)]);
        assert_eq!(w.merging_blocks(), 0);
        assert_eq!(w.total_merge_parents(), 0);
    }

    #[test]
    fn colouring_inconsistency_is_detected() {
        // blue_score advances every block but every block is flagged red.
        let blocks: Vec<BlockFact> = (1..=5)
            .map(|h| BlockFact {
                height: h,
                producer: [1u8; 32],
                merge_parent_count: 0,
                blue_score: Some(h),
                is_blue: Some(false),
            })
            .collect();
        let w = BlockWindow::new(blocks);
        assert_eq!(w.colouring_inconsistencies(), 4);
    }

    #[test]
    fn network_size_provenance_propagates() {
        assert_eq!(
            NetworkSize::Placeholder(50).provenance(),
            Provenance::Placeholder
        );
        assert_eq!(NetworkSize::Known(12).provenance(), Provenance::Measured);
        assert_eq!(NetworkSize::Estimated(31).provenance(), Provenance::Derived);
    }

    #[test]
    fn quiescent_node_is_flagged() {
        let o = Observables::default();
        assert!(o.is_quiescent());

        let mut o2 = Observables::default();
        o2.current.local_height = 10;
        assert!(!o2.is_quiescent());
    }

    #[test]
    fn counters_saturate_on_restart() {
        let mut o = Observables::default();
        o.previous.p2p_bytes_in = 1_000;
        o.current.p2p_bytes_in = 5; // node restarted, counter reset
        assert_eq!(o.bytes_in_delta(), 0);
    }
}
