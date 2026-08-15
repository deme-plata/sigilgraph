//! fair_order_experiment.rs — Phase G, part 2 (SIGIL_BRAIDPOOL_v1_1.md §15):
//! *"Evaluate Tilikum-style or post-consensus visibility approaches
//! separately."*
//!
//! **What this module IS:** a small, self-contained, deterministic
//! measurement of ONE narrow, well-documented ordering-bias mechanism —
//! deciding same-round ties by validator/creator identity — compared
//! against ONE narrow mitigation — deciding same-round ties by batch
//! content instead. Both the bias and the fix are measured, not asserted:
//! every claim below has a test backing it.
//!
//! **What this module IS NOT, stated plainly so it can never be
//! mis-cited:** this is not an implementation of Tilikum, Themis, Aequitas,
//! Multi-Round-Visibility (MRV), or any other specific published
//! fair-ordering protocol. Those constructions use richer machinery this
//! module does not attempt — causal-history cliques, Byzantine agreement on
//! relative order across multiple honest validators' observed arrival
//! times, or receive-order commitments gossiped and cross-checked BEFORE a
//! batch is finalized. This module only ever looks at metadata for
//! already-produced batches on ONE node, with no cross-validator agreement
//! step at all. §15's instruction was to evaluate those approaches
//! separately, not to substitute for them, and this module does not
//! pretend otherwise. Treat it as a demonstration that identity-based
//! tie-breaks are the mechanism of the bias the doc warns about — a
//! starting point for that separate evaluation, not its conclusion.
//!
//! **Not wired into anything.** [`crate::backend::MempoolBackend`] does not
//! call any function here. This is standalone crate machinery, same caution
//! pattern as every other phase.

use crate::order_meta::BatchOrderMetaV1;
use crate::types::WorkerId;

/// The KNOWN-BAD baseline §15 warns about: within a round, ties are broken
/// by ascending creator/worker index. Sorts in place. Primary key
/// `first_seen_round` (visibility always dominates); tie-break is the
/// biased one.
pub fn order_naive_index_tiebreak(metas: &mut [BatchOrderMetaV1]) {
    metas.sort_by(|a, b| a.first_seen_round.cmp(&b.first_seen_round).then(a.creator.0.cmp(&b.creator.0)));
}

/// The minimal mitigation this module actually measures: same primary key
/// (`first_seen_round`), but the tie-break is the batch's own `tx_root` —
/// content the creator does not fully control at will (it's the Merkle
/// root over the batch's actual transactions) — instead of the creator's
/// identity. This removes identity as an ordering signal; it does NOT by
/// itself prove Byzantine-robust fairness against an adversary who can
/// grind `tx_root` values, which is exactly the kind of concern the cited
/// literature (Tilikum/MRV/etc.) is built to address and this module is not.
pub fn order_content_tiebreak(metas: &mut [BatchOrderMetaV1]) {
    metas.sort_by(|a, b| a.first_seen_round.cmp(&b.first_seen_round).then(a.tx_root.cmp(&b.tx_root)));
}

/// Deterministic synthetic test/bench corpus: exactly one [`BatchOrderMetaV1`]
/// per worker index `0..worker_count`, all sharing the same
/// `first_seen_round`, each with a `tx_root` derived from
/// `BLAKE3(domain || seed || round || worker_index)`. Content-derived, not
/// an external RNG dependency — matches the crate's existing
/// BLAKE3-for-determinism convention (see `worker::epoch_salted_index`).
/// One batch per worker isolates "does creator identity decide the winner?"
/// from sampling luck: every cohort has a full, identical set of
/// participants, so any consistent bias shows up every single time, not
/// just on average.
pub fn synthetic_tie_cohort(seed: &[u8], round: u64, worker_count: u16) -> Vec<BatchOrderMetaV1> {
    (0..worker_count)
        .map(|w| {
            let mut h = blake3::Hasher::new();
            h.update(b"SIGIL/ORDERBIAS/SYNTH/V1");
            h.update(seed);
            h.update(&round.to_le_bytes());
            h.update(&w.to_le_bytes());
            let digest = h.finalize();
            BatchOrderMetaV1 {
                creator: WorkerId(w),
                epoch: 0,
                sequence: w as u64,
                first_seen_round: round,
                tx_root: *digest.as_bytes(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naive_tiebreak_always_favors_the_lowest_creator_index() {
        // 50 independent cohorts, each with 8 workers all tied on round —
        // under the naive scheme, worker 0 is present in every cohort by
        // construction, and the tie-break is purely `creator.0` ascending,
        // so it MUST win every single time, not just on average.
        for i in 0..50u64 {
            let mut cohort = synthetic_tie_cohort(b"bias-check", i, 8);
            order_naive_index_tiebreak(&mut cohort);
            assert_eq!(cohort[0].creator, WorkerId(0), "cohort {i}: naive scheme must always place worker 0 first when all workers tie on round");
        }
    }

    #[test]
    fn content_tiebreak_winner_is_not_determined_by_creator_index() {
        // Two metas, same round, DIFFERENT creators, but with their
        // tx_roots swapped relative to a baseline run. If content decides
        // the order, swapping which creator holds which tx_root must swap
        // who wins — proving the winner tracks the CONTENT, not the
        // identity attached to it.
        let round = 5;
        let root_lo = [0u8; 32];
        let mut root_hi = [0u8; 32];
        root_hi[31] = 1;

        let mut baseline = vec![
            BatchOrderMetaV1 { creator: WorkerId(0), epoch: 0, sequence: 0, first_seen_round: round, tx_root: root_lo },
            BatchOrderMetaV1 { creator: WorkerId(1), epoch: 0, sequence: 0, first_seen_round: round, tx_root: root_hi },
        ];
        order_content_tiebreak(&mut baseline);
        assert_eq!(baseline[0].creator, WorkerId(0), "worker 0 holds the lexicographically smaller root, so it wins");

        let mut swapped = vec![
            BatchOrderMetaV1 { creator: WorkerId(0), epoch: 0, sequence: 0, first_seen_round: round, tx_root: root_hi },
            BatchOrderMetaV1 { creator: WorkerId(1), epoch: 0, sequence: 0, first_seen_round: round, tx_root: root_lo },
        ];
        order_content_tiebreak(&mut swapped);
        assert_eq!(swapped[0].creator, WorkerId(1), "the SAME content (root_lo) now belongs to worker 1, and worker 1 wins — the win follows the content, not the identity");
    }

    #[test]
    fn first_seen_round_dominates_regardless_of_tiebreak_scheme() {
        // A batch from an earlier round beats a batch from a later round
        // under EITHER scheme, even when its tie-break key would otherwise
        // lose — visibility ordering is preserved; only within-round ties
        // are handled differently.
        let earlier = BatchOrderMetaV1 { creator: WorkerId(9), epoch: 0, sequence: 0, first_seen_round: 1, tx_root: [0xffu8; 32] };
        let later = BatchOrderMetaV1 { creator: WorkerId(0), epoch: 0, sequence: 0, first_seen_round: 2, tx_root: [0x00u8; 32] };

        let mut a = vec![later, earlier];
        order_naive_index_tiebreak(&mut a);
        assert_eq!(a[0].first_seen_round, 1);

        let mut b = vec![later, earlier];
        order_content_tiebreak(&mut b);
        assert_eq!(b[0].first_seen_round, 1);
    }

    #[test]
    fn content_tiebreak_distributes_wins_roughly_evenly_across_workers() {
        // The statistical claim: over many independent tie cohorts, no
        // single worker index systematically wins under the content
        // scheme. 2000 cohorts of 8 workers each; uniform would be ~250
        // wins/worker. Bounds are loose (100..450, i.e. 40%-180% of
        // uniform) specifically so this test does not flake — it exists to
        // catch a GROSS bias (like the naive scheme's 100%-to-worker-0),
        // not to certify statistical fairness to a tight tolerance.
        let worker_count = 8u16;
        let cohorts = 2000u64;
        let mut wins = vec![0u32; worker_count as usize];
        for i in 0..cohorts {
            let mut cohort = synthetic_tie_cohort(b"distribution-check", i, worker_count);
            order_content_tiebreak(&mut cohort);
            wins[cohort[0].creator.0 as usize] += 1;
        }
        for (w, &count) in wins.iter().enumerate() {
            assert!(
                (100..450).contains(&count),
                "worker {w} won {count}/{cohorts} ties under content tiebreak — expected roughly uniform (~250), got a gross skew"
            );
        }
    }
}
