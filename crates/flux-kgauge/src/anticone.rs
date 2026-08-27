//! The **other** k: DAG-Knight's anticone parameter.
//!
//! This module exists because two completely different quantities in this
//! system are both called "k", and confusing them produces confident nonsense.
//!
//! | symbol | what it is | small value means |
//! |---|---|---|
//! | `k` / `kappa` (here) | the DAG-Knight anticone bound — how many blocks may be concurrent with a blue block | the network is fast and blocks are nearly serial |
//! | `K` (the K-gauge) | a composite *stress* score from Eq. 10 | the node is healthy |
//!
//! They move in opposite directions under the same conditions and they are not
//! related by any formula. A low anticone `k` is a statement about latency
//! versus block interval. A low `K` is a statement about operational health.
//! Both being low at once is the normal state of a small, fast, well-connected
//! network — which is why this module reports them side by side but never
//! mixes them.
//!
//! ## What is actually measurable here
//!
//! The true anticone of a block requires traversing the DAG. What a block
//! header cheaply exposes is its **merge-parent count**: how many extra parents
//! it pulled in beyond the selected one. That is a *lower bound* on the width
//! the producer observed, and it is exact in the one case that matters most —
//! zero merge parents everywhere means the DAG is a straight chain and the
//! observed anticone is empty.

use serde::{Deserialize, Serialize};

use crate::observables::BlockWindow;
use crate::provenance::Provenance;

/// What shape the recent history actually has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DagShape {
    /// No block merged an extra parent. Every DAG-specific mechanism —
    /// blue-set selection, anticone penalties, the whole PHANTOM apparatus — is
    /// running on a structure with nothing to order. This is not a fault, but
    /// it does mean DAG-derived health metrics carry no information.
    LinearChain,
    /// Some merging, within the configured `kappa`.
    NarrowDag,
    /// Observed width exceeds `kappa`. Blocks are being produced faster than
    /// they propagate; DAG-Knight's tolerance is being tested.
    WideDag,
    /// No blocks to look at.
    Unknown,
}

/// The anticone measurement for a window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnticoneMeasure {
    /// Largest merge-parent count seen. A lower bound on observed anticone width.
    pub k_observed_max: u32,
    /// Mean merge-parent count across the window.
    pub k_observed_mean: f64,
    /// Fraction of blocks that merged anything at all.
    pub merge_fraction: f64,
    /// The configured tolerance this is judged against.
    pub kappa: f64,
    pub shape: DagShape,
    /// Distinct block producers in the window. One producer means the DAG has
    /// no independent contributors to order, whatever its shape.
    pub distinct_producers: usize,
    /// Blocks whose reported colouring contradicts their blue-score progression.
    pub colouring_inconsistencies: usize,
    pub blocks: usize,
    pub provenance: Provenance,
}

impl AnticoneMeasure {
    /// True when nothing in this window can inform a DAG-shaped metric: a
    /// straight chain from a single producer.
    pub fn dag_metrics_are_vacuous(&self) -> bool {
        matches!(self.shape, DagShape::LinearChain | DagShape::Unknown)
            || self.distinct_producers <= 1
    }

    /// Headline sentence for a human.
    pub fn summary(&self) -> String {
        match self.shape {
            DagShape::Unknown => "no blocks observed — DAG shape unknown".to_string(),
            DagShape::LinearChain => format!(
                "straight chain: {} blocks, 0 merge-parents, {} producer(s) — observed anticone k = 0",
                self.blocks, self.distinct_producers
            ),
            DagShape::NarrowDag => format!(
                "narrow DAG: max observed k = {} (kappa = {:.0}), {:.1}% of blocks merged, {} producer(s)",
                self.k_observed_max,
                self.kappa,
                self.merge_fraction * 100.0,
                self.distinct_producers
            ),
            DagShape::WideDag => format!(
                "WIDE DAG: max observed k = {} exceeds kappa = {:.0} — blocks outrunning propagation",
                self.k_observed_max, self.kappa
            ),
        }
    }
}

/// Measure the observed anticone width over a window.
pub fn measure(window: &BlockWindow, kappa: f64) -> AnticoneMeasure {
    let blocks = window.len();
    if blocks == 0 {
        return AnticoneMeasure {
            k_observed_max: 0,
            k_observed_mean: 0.0,
            merge_fraction: 0.0,
            kappa,
            shape: DagShape::Unknown,
            distinct_producers: 0,
            colouring_inconsistencies: 0,
            blocks: 0,
            provenance: Provenance::Unavailable,
        };
    }

    let k_observed_max = window
        .blocks
        .iter()
        .map(|b| b.merge_parent_count)
        .max()
        .unwrap_or(0);
    let total_merges = window.total_merge_parents();
    let k_observed_mean = total_merges as f64 / blocks as f64;
    let merging = window.merging_blocks();
    let merge_fraction = merging as f64 / blocks as f64;

    let shape = if merging == 0 {
        DagShape::LinearChain
    } else if (k_observed_max as f64) > kappa {
        DagShape::WideDag
    } else {
        DagShape::NarrowDag
    };

    AnticoneMeasure {
        k_observed_max,
        k_observed_mean,
        merge_fraction,
        kappa,
        shape,
        distinct_producers: window.distinct_producers(),
        colouring_inconsistencies: window.colouring_inconsistencies(),
        blocks,
        // Merge-parent counts are read straight off blocks, but they are a
        // lower bound on the true anticone, so the derived width is Derived.
        provenance: Provenance::Derived,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observables::BlockFact;

    fn win(specs: &[(u64, u8, u32)]) -> BlockWindow {
        BlockWindow::new(
            specs
                .iter()
                .map(|&(h, p, m)| BlockFact {
                    height: h,
                    producer: [p; 32],
                    merge_parent_count: m,
                    blue_score: Some(h),
                    is_blue: Some(true),
                })
                .collect(),
        )
    }

    #[test]
    fn straight_chain_is_detected() {
        let w = win(&[(1, 1, 0), (2, 1, 0), (3, 1, 0)]);
        let m = measure(&w, 18.0);
        assert_eq!(m.shape, DagShape::LinearChain);
        assert_eq!(m.k_observed_max, 0);
        assert!(m.dag_metrics_are_vacuous());
        assert!(m.summary().contains("straight chain"));
    }

    #[test]
    fn single_producer_makes_dag_metrics_vacuous_even_when_merging() {
        let w = win(&[(1, 1, 2), (2, 1, 1), (3, 1, 3)]);
        let m = measure(&w, 18.0);
        assert_eq!(m.shape, DagShape::NarrowDag);
        assert_eq!(m.distinct_producers, 1);
        assert!(m.dag_metrics_are_vacuous());
    }

    #[test]
    fn narrow_dag_with_several_producers_is_informative() {
        let w = win(&[(1, 1, 1), (2, 2, 2), (3, 3, 1), (4, 1, 0)]);
        let m = measure(&w, 18.0);
        assert_eq!(m.shape, DagShape::NarrowDag);
        assert_eq!(m.distinct_producers, 3);
        assert!(!m.dag_metrics_are_vacuous());
        assert_eq!(m.k_observed_max, 2);
        assert!((m.merge_fraction - 0.75).abs() < 1e-12);
    }

    #[test]
    fn width_beyond_kappa_is_flagged() {
        let w = win(&[(1, 1, 25), (2, 2, 3)]);
        let m = measure(&w, 18.0);
        assert_eq!(m.shape, DagShape::WideDag);
        assert!(m.summary().contains("WIDE DAG"));
    }

    #[test]
    fn empty_window_is_unknown_not_linear() {
        let m = measure(&BlockWindow::default(), 18.0);
        assert_eq!(m.shape, DagShape::Unknown);
        assert_eq!(m.provenance, Provenance::Unavailable);
    }

    #[test]
    fn colouring_inconsistency_surfaces() {
        let w = BlockWindow::new(
            (1..=4)
                .map(|h| BlockFact {
                    height: h,
                    producer: [1u8; 32],
                    merge_parent_count: 0,
                    blue_score: Some(h),
                    is_blue: Some(false),
                })
                .collect(),
        );
        let m = measure(&w, 18.0);
        assert_eq!(m.colouring_inconsistencies, 3);
    }

    #[test]
    fn mean_width_is_computed_over_all_blocks() {
        let w = win(&[(1, 1, 4), (2, 2, 0), (3, 3, 0), (4, 4, 0)]);
        let m = measure(&w, 18.0);
        assert!((m.k_observed_mean - 1.0).abs() < 1e-12);
    }
}
