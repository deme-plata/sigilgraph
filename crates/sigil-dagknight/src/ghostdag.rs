//! GHOSTDAG-style k-cluster blue/red block coloring — the **v2** ordering
//! lane, opt-in via [`crate::BraidConfig::ghostdag_k`] (env
//! `SIGIL_DAG_GHOSTDAG_K`). v1 (deterministic braid linearization, unchanged)
//! stays the default whenever this is unset.
//!
//! **Honest naming — read before quoting any claim about this module.**
//! This IS real GHOSTDAG-family consensus: selected-parent-by-blue-score
//! (not by height), and a greedy admission test that colors a candidate
//! block BLUE only if doing so keeps every currently-blue block's
//! concurrent-block count (its "blue anticone") within the fixed parameter
//! `k`; a candidate that would violate that bound for itself or for any
//! already-blue block is colored RED. This is the actual security substrate
//! DagKnight-family chains use to tell honest concurrent work apart from an
//! adversary's parallel branch.
//!
//! This is **NOT DagKnight-the-paper**: `k` here is a fixed, operator-chosen
//! parameter (like classic GHOSTDAG/PHANTOM), not the parameterless,
//! network-delay-adaptive min-cut `k` that the DAG KNIGHT paper (Sompolinsky
//! & Zohar) derives on the fly. Making `k` adaptive is a further, separate
//! increment and is not promised here.
//!
//! Blue **work** (difficulty-weighted selection) is now implemented — see
//! [`WorkPolicy`] — but the DEFAULT is `UniformCount`, under which `blue_work`
//! is numerically identical to blue score and selection is byte-for-byte what
//! it always was. That default is not timidity: `header.difficulty` on this
//! chain is an EXPONENT and is 0 on 99.83% of blocks (measured 2026-08-28,
//! 7 real solves in 4096), so weighting by it today would be worse than
//! counting. Activating `Exponential` is consensus-affecting and blocked on a
//! prerequisite — every block carrying a truthful work claim. `Braid`'s
//! finality window (`final_depth`) still counts in **height**, not blue score
//! or work — see `Braid::computed_final`. Explicit, scoped follow-ups.
//!
//! ## Algorithm
//!
//! For a new block `B` with (deduplicated, resident) parent set `P`:
//!
//! 1. **Selected parent** = the member of `P` with the highest blue score,
//!    tie-broken by the smaller hash (mirrors the tie-break convention used
//!    throughout this crate).
//! 2. **mergeSet(B)** = `past(B) \ (past(selected_parent) ∪ {selected_parent})`
//!    — the ancestors `B` pulls in via its *other* parents, restricted to the
//!    resident window (via [`crate::bitset::BitfieldDag::past_diff_hashes`]).
//! 3. Process mergeSet in ascending `(blue_score, hash)` order (every member
//!    is a strict ancestor of `B`, so its data was already computed).
//! 4. For each candidate `X`: let `blueAnticone` = `anticone(X) ∩ blueSet`.
//!    `X` is admitted BLUE iff `|blueAnticone| ≤ k` AND, for every `b` in
//!    `blueAnticone`, admitting `X` would not push `b`'s own blue-anticone
//!    count past `k` (checked via `anticone(b) ∩ blueSet`, using the
//!    anticone-symmetry `b ∈ anticone(X) ⟺ X ∈ anticone(b)`). Otherwise `X`
//!    is RED.
//! 5. `blue_score(B) = blue_score(selected_parent) + 1 (for the selected
//!    parent itself) + |new blues|`.
//!
//! ## Complexity note (engineering simplification, not a shortcut on
//! correctness)
//!
//! A block's full historical blue set is reconstructed on demand by walking
//! its selected-parent chain within the resident window and unioning each
//! ancestor's small local `merge_set_blues` ([`GhostdagStore::blue_set_of`]).
//! This is O(resident chain length) per insert — the same asymptotic class
//! `BitfieldDag` already pays per insert for its past-set unions, so it is
//! not a complexity regression relative to what this crate already accepts.
//! A future perf pass (memoizing reconstructed blue sets, invalidated on
//! cleanup) is the natural v2.1 if the *measured* live cost ever demands it
//! — per the sigil skill's Rule 0, that tuning only happens after a real
//! measurement, not speculatively.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;

use sigil_header::BlockHash;

use crate::bitset::{BitfieldDag, VertexBitfield};

/// GHOSTDAG data computed for one block: its selected parent, the blue/red
/// split of everything its non-selected parents pulled in, and its blue
/// score (the v2 chain-selection metric).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockGhostdagData {
    /// The parent (among all of this block's parents) with the highest blue
    /// score. `None` only for a block with no known/resident parents
    /// (genesis, or a base-anchored seed).
    pub selected_parent: Option<BlockHash>,
    /// mergeSet members colored BLUE this block (excludes the selected
    /// parent itself, which is implicitly blue).
    pub merge_set_blues: Vec<BlockHash>,
    /// mergeSet members colored RED this block — concurrent work the
    /// k-cluster bound could not absorb.
    pub merge_set_reds: Vec<BlockHash>,
    /// `blue_score(selected_parent) + 1 + |merge_set_blues|`.
    pub blue_score: u64,
    /// Accumulated WORK of this block's full blue set, under the store's
    /// [`WorkPolicy`]. Under `UniformCount` this equals `blue_score` exactly —
    /// that equivalence is asserted by test, and is what makes the default a
    /// provable no-op. `u128` because `Exponential` sums powers of two and a
    /// `u64` would overflow well inside a normal window.
    pub blue_work: u128,
    /// v2.1 (2026-08-19 — measured via real `perf` profiling on a deep
    /// catch-up node, per this module's own "complexity note" above, which
    /// named exactly this fix and exactly this trigger condition before it
    /// was ever a problem). The FULL historical blue set as of this block —
    /// `{this block} ∪ new_blues(this block) ∪ selected_parent's own
    /// full_blue_set` — computed once here instead of reconstructed by
    /// walking the entire selected-parent chain (up to the whole resident
    /// window, thousands of blocks) on every `blue_set_of`/`is_blue` call.
    /// Correct by construction (each block's full set is built directly
    /// from its already-correct parent's full set, not re-derived), and
    /// evicted in lockstep with the rest of this struct by `forget()` — no
    /// separate cache-invalidation path to get wrong. A late-joining node
    /// doing a deep catch-up used to spend the overwhelming majority of its
    /// CPU here (81% of sampled time, ~36% of that shows up as pure hashing
    /// underneath the repeated HashSet reconstruction) while producing zero
    /// visible sync progress.
    ///
    /// Stored as a [`VertexBitfield`] (indexed via [`BitfieldDag::index_of`]),
    /// NOT a `HashSet<BlockHash>` — the first version of this fix used a
    /// HashSet and OOM-killed a real catch-up node at ~15GB in under a
    /// minute: consecutive blocks' full blue sets are ~identical (each adds
    /// only a handful of members), but each block still stored an
    /// independent full clone, so total memory scaled as O(window²) at
    /// ~80+ bytes per member. A bitfield costs a FIXED `window/8` bytes per
    /// stored set REGARDLESS of how many members are set, so the same
    /// O(window²) count of stored sets costs O(window²/8) bytes total — the
    /// exact tradeoff `BitfieldDag` itself already documents and accepts
    /// for its own past/future sets at this same window size (its own doc
    /// comment: "memory is O(window²/8) bytes" — ~32MB at max_window=16384,
    /// nowhere near OOM territory).
    full_blue_set: VertexBitfield,
}


/// How much a block weighs when comparing two branches.
///
/// GHOSTDAG's published selection rule compares blue **score** — a COUNT of
/// blue blocks. That is the correct metric only when every block represents
/// roughly the same work. Two producers with very different hashpower break
/// that assumption: their blocks count the same, so "heaviest branch" stops
/// tracking actual work. That is a fork hazard, and it is why this type exists.
///
/// ## Why the obvious fix is not the default
///
/// The obvious weight is `header.difficulty`. On this chain, today, that would
/// be **worse than counting**, for two measured reasons:
///
/// 1. **It is an exponent, not work.** `difficulty` is `solve.bits`, fed to
///    `target_from_bits`, so the work it denotes is `2^bits`. Summing `bits`
///    would treat 25 as 1.04x of 24 instead of 2x.
/// 2. **It is zero on almost every block.** Measured 2026-08-28 against the
///    live chain: 7 of 4096 recent blocks carried a real solve (0.17%). The
///    other 4089 are producer free-run mints with `difficulty = 0` AND
///    `vdf_proof.t = 0` — they carry no proof of work at all. Weighting by it
///    would give 99.83% of blocks zero weight and let a handful of blocks
///    decide fork choice outright.
///
/// So the plumbing is built here and the metric is switchable, but the default
/// reproduces today's behaviour EXACTLY. Activating a real work metric is
/// blocked on a separate prerequisite: every block must carry a truthful
/// statement of its own work. Until then `Exponential` is a loaded gun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkPolicy {
    /// Every block weighs 1. `blue_work` is then numerically identical to
    /// blue score, so selection is byte-for-byte what it was before this type
    /// existed. **The default**, and a strict no-op.
    UniformCount,
    /// `weight = 2^min(difficulty, 63)`, floored at `base` for blocks that
    /// declare no difficulty. `base` keeps an unproven block from weighing
    /// zero, which is what makes this survivable at all on a chain where most
    /// blocks declare nothing.
    ///
    /// ⚠️ Consensus-affecting. Every node must switch together or the network
    /// forks on the metric itself. Do not enable without a coordinated
    /// activation.
    Exponential {
        /// Floor weight for a block that declares `difficulty == 0`. Must be
        /// >= 1: a zero-weight block is invisible to branch comparison.
        base: u64,
    },
}

impl Default for WorkPolicy {
    fn default() -> Self {
        WorkPolicy::UniformCount
    }
}

impl WorkPolicy {
    /// Weight of a single block. Never returns 0 — a zero-weight block is
    /// invisible to branch comparison, which is the failure this whole type is
    /// meant to avoid.
    pub fn weight(self, difficulty: u64) -> u128 {
        match self {
            WorkPolicy::UniformCount => 1,
            WorkPolicy::Exponential { base } => {
                let floor = (base as u128).max(1);
                if difficulty == 0 {
                    floor
                } else {
                    (1u128 << difficulty.min(63)).max(floor)
                }
            }
        }
    }
}

/// Incremental GHOSTDAG blue/red coloring store, parameterized by the fixed
/// cluster bound `k`. Lives alongside [`crate::braid::Braid`]'s window; the
/// caller (`Braid`) is responsible for calling [`GhostdagStore::forget`] in
/// lockstep with its own window cleanup so the two stay bounded together.
pub struct GhostdagStore {
    k: u32,
    policy: WorkPolicy,
    data: HashMap<BlockHash, BlockGhostdagData>,
}

impl GhostdagStore {
    /// New empty store for cluster parameter `k`.
    pub fn new(k: u32) -> Self {
        Self {
            k,
            policy: WorkPolicy::default(),
            data: HashMap::new(),
        }
    }

    /// New store with an explicit work policy.
    ///
    /// ⚠️ Anything other than [`WorkPolicy::UniformCount`] changes which branch
    /// wins and is therefore consensus-affecting — every node must agree.
    pub fn with_policy(k: u32, policy: WorkPolicy) -> Self {
        Self { k, policy, data: HashMap::new() }
    }

    /// The configured work policy.
    pub fn policy(&self) -> WorkPolicy {
        self.policy
    }

    /// Accumulated blue work of `h`, or 0 if unknown.
    pub fn blue_work(&self, h: &BlockHash) -> u128 {
        self.data.get(h).map(|d| d.blue_work).unwrap_or(0)
    }

    /// The configured cluster bound.
    pub fn k(&self) -> u32 {
        self.k
    }

    /// GHOSTDAG data for a resident block, if computed.
    pub fn get(&self, h: &BlockHash) -> Option<&BlockGhostdagData> {
        self.data.get(h)
    }

    /// Blue score of `h`, or 0 if unknown (genesis convention: blue score 0).
    pub fn blue_score(&self, h: &BlockHash) -> u64 {
        self.data.get(h).map(|d| d.blue_score).unwrap_or(0)
    }

    /// The full blue set of `block` as a compact bitfield (indexed via
    /// `BitfieldDag::index_of` — see `full_blue_set`'s doc for why a
    /// bitfield, not a `HashSet<BlockHash>`): itself, plus every ancestor
    /// reachable by walking `selected_parent` pointers within the resident
    /// window, plus each of those ancestors' local `merge_set_blues`. v2.1:
    /// a plain memoized clone instead of a from-scratch chain walk. Unknown/
    /// non-resident blocks get an empty bitfield sized to the DAG's current
    /// capacity, matching the old walk's behavior of returning nothing for
    /// a block with zero resident data.
    fn blue_set_of(&self, dag: &BitfieldDag, block: &BlockHash) -> VertexBitfield {
        let mut bf = self
            .data
            .get(block)
            .map(|d| d.full_blue_set.clone())
            .unwrap_or_else(|| VertexBitfield::new(dag.capacity()));
        // 2026-08-28: `grow_to` is load-bearing, and its absence was a live
        // bug that silently disabled GHOSTDAG colouring for every chain
        // longer than 64 blocks.
        //
        // Every block after the seed inherits its blue set by CLONING its
        // selected parent's, and a clone preserves the parent's *width*.
        // Nothing in this store ever widened it, so the width stayed frozen
        // at whatever `dag.capacity()` was when the seed block was computed —
        // one 64-bit word. `BitfieldDag::ensure_capacity` does sweep
        // `grow_to` across its own `past_sets`/`future_sets`/`parent_sets`,
        // but `full_blue_set` lives in THIS struct and was never in that
        // sweep, even though it is indexed against the same index space.
        //
        // `VertexBitfield::set` and `::test` both fail silently out of range
        // (no-op, and `false`, respectively). So from vertex index 64 onward:
        //   - `compute` could not set a block's own bit -> a block was not in
        //     its own blue set,
        //   - `is_blue` answered `false` for every query,
        //   - and the k-cluster admission test below, which reads
        //     `blue_set.test(i)`, saw a saturated all-ones 64-bit window
        //     instead of the real blue set, so it was neither counting the
        //     right members nor enforcing `k` against them.
        //
        // Symptom that surfaced it: `/v1/dagknight/recent` returning 200/200
        // blocks with `is_blue = false` while `blue_score` advanced by 1 on
        // every one of them — two statements that cannot both be true of the
        // same chain. `blue_score` was right; the colouring was not.
        //
        // `index_map.capacity()` is monotonic non-decreasing, so growing here
        // is a no-op in the steady state and can never shrink a set.
        bf.grow_to(dag.capacity());
        bf
    }

    /// True iff `target` is blue relative to `tip` (on `tip`'s
    /// selected-parent chain, or in some chain ancestor's `merge_set_blues`).
    /// Looks directly at the memoized bitfield without cloning it (unlike
    /// routing through `blue_set_of`, which exists for `compute`'s need to
    /// further extend a copy) — needs `dag` only to translate `target` into
    /// its compact index.
    pub fn is_blue(&self, dag: &BitfieldDag, tip: &BlockHash, target: &BlockHash) -> bool {
        let Some(target_idx) = dag.index_of(target) else { return false };
        let Some(d) = self.data.get(tip) else { return false };
        // Width invariant, maintained by `blue_set_of`'s `grow_to`: a stored
        // blue set is always at least as wide as the index space was when it
        // was computed, and capacity is monotonic, so the TIP's set — the most
        // recently computed one — addresses every currently resident vertex.
        // Without that invariant `test` returns `false` for out-of-range
        // indices, which is indistinguishable from an honest "not blue" and is
        // exactly how this went unnoticed. See `blue_set_of`.
        debug_assert!(
            (target_idx as usize) < d.full_blue_set.width(),
            "blue set of tip is {} bits wide but target index is {target_idx}: \
             a narrow set silently answers `false` for every query past its width",
            d.full_blue_set.width()
        );
        d.full_blue_set.test(target_idx)
    }

    /// Select the candidate with the highest blue score, tie-broken by the
    /// smaller hash — the v2 analog of `Braid`'s v1 max-height/min-hash tip
    /// selection.
    /// Heaviest branch by accumulated blue WORK, tie-broken by the smaller
    /// hash.
    ///
    /// Under [`WorkPolicy::UniformCount`] — the default — `blue_work` is
    /// numerically equal to `blue_score`, so this is exactly the blue-score
    /// comparison it replaces. `select_tip_matches_blue_score_under_uniform`
    /// asserts that equivalence on a DAG with real concurrency, so switching
    /// this comparison from score to work cannot silently change consensus
    /// while the default policy is in force.
    pub fn select_tip<'a>(&self, candidates: impl Iterator<Item = &'a BlockHash>) -> Option<BlockHash> {
        candidates.copied().reduce(|a, b| {
            let wa = self.blue_work(&a);
            let wb = self.blue_work(&b);
            if wb > wa || (wb == wa && b < a) {
                b
            } else {
                a
            }
        })
    }

    /// Compute and store GHOSTDAG data for `block`, given its deduplicated,
    /// resident parent set. `dag.add_vertex(block, ..)` MUST already have
    /// run (this reads `block`'s past set). Returns the stored data.
    pub fn compute(
        &mut self,
        dag: &BitfieldDag,
        block: BlockHash,
        parents: &[BlockHash],
    ) -> &BlockGhostdagData {
        if parents.is_empty() {
            let block_idx = dag.index_of(&block)
                .expect("dag.add_vertex(block, ..) must already have run");
            let mut full_blue_set = VertexBitfield::new(dag.capacity());
            full_blue_set.set(block_idx);
            // Mirror the genesis blue_score convention (`blue_score: 0`): the
            // seed block is the common ancestor of everything, so it cancels
            // out of every branch comparison and contributes no work. Giving it
            // weight here would break the `blue_work == blue_score` identity at
            // the very first child — which is exactly what the test caught.
            let seed_work: u128 = 0;
            self.data.insert(
                block,
                BlockGhostdagData {
                    selected_parent: None,
                    merge_set_blues: Vec::new(),
                    merge_set_reds: Vec::new(),
                    blue_score: 0,
                    blue_work: seed_work,
                    full_blue_set,
                },
            );
            return self.data.get(&block).expect("just inserted");
        }

        // 1. Selected parent: highest blue score, tie-break smaller hash.
        let selected_parent = parents
            .iter()
            .copied()
            .reduce(|a, b| {
                let sa = self.blue_score(&a);
                let sb = self.blue_score(&b);
                if sb > sa || (sb == sa && b < a) {
                    b
                } else {
                    a
                }
            })
            .expect("non-empty parents");

        // 2. mergeSet = past(block) \ (past(selected_parent) ∪ {selected_parent}).
        let mut merge_set = dag
            .past_diff_hashes(&block, &selected_parent)
            .unwrap_or_default();

        // 3. Deterministic processing order over already-scored ancestors.
        merge_set.sort_by(|a, b| {
            self.blue_score(a)
                .cmp(&self.blue_score(b))
                .then_with(|| a.cmp(b))
        });

        // 4. Greedy k-cluster admission. Membership tests go through the
        // vertex index (`dag.index_of`), not the hash directly — `blue_set`
        // is now a `VertexBitfield` (see `full_blue_set`'s doc for why).
        let mut blue_set = self.blue_set_of(dag, &selected_parent);
        let mut new_blues = Vec::new();
        let mut new_reds = Vec::new();
        for cand in merge_set {
            let cand_anticone = dag.anticone_hashes(&cand).unwrap_or_default();
            let blue_anticone: Vec<BlockHash> = cand_anticone
                .iter()
                .copied()
                .filter(|h| dag.index_of(h).is_some_and(|i| blue_set.test(i)))
                .collect();

            let admits = blue_anticone.len() as u32 <= self.k
                && blue_anticone.iter().all(|anc| {
                    let anc_anticone = dag.anticone_hashes(anc).unwrap_or_default();
                    let anc_blue_count = anc_anticone
                        .iter()
                        .filter(|h| dag.index_of(h).is_some_and(|i| blue_set.test(i)))
                        .count() as u32;
                    // +1 for `cand` itself joining anc's blue anticone
                    // (anticone-symmetry: cand ∈ anticone(anc) here).
                    anc_blue_count + 1 <= self.k
                });

            if admits {
                if let Some(idx) = dag.index_of(&cand) {
                    blue_set.set(idx);
                }
                new_blues.push(cand);
            } else {
                new_reds.push(cand);
            }
        }

        let blue_score = self.blue_score(&selected_parent) + 1 + new_blues.len() as u64;
        // Work accumulates over exactly the same set the score counts: the
        // selected parent's accumulated work, plus this block, plus every
        // member this block newly coloured blue. Keeping the two in lockstep is
        // what makes `UniformCount` provably equal to the count.
        let block_idx = dag.index_of(&block)
            .expect("dag.add_vertex(block, ..) must already have run");
        let blue_work = {
            let mut w = self.blue_work(&selected_parent);
            w = w.saturating_add(self.policy.weight(dag.difficulty_at(block_idx)));
            for nb in &new_blues {
                let d = dag.index_of(nb).map(|i| dag.difficulty_at(i)).unwrap_or(0);
                w = w.saturating_add(self.policy.weight(d));
            }
            w
        };

        // `blue_set` already equals full_blue_set(selected_parent) ∪ new_blues
        // (built above for the admission test) — reuse it directly instead of
        // reconstructing; just add `block` itself, matching what the old
        // from-scratch walk would have produced for this exact block.
        let mut full_blue_set = blue_set;
        full_blue_set.set(block_idx);

        self.data.insert(
            block,
            BlockGhostdagData {
                selected_parent: Some(selected_parent),
                merge_set_blues: new_blues,
                merge_set_reds: new_reds,
                blue_score,
                blue_work,
                full_blue_set,
            },
        );
        self.data.get(&block).expect("just inserted")
    }

    /// Drop stored data for hashes the caller has removed from its own
    /// resident window (call with the same hashes `Braid::cleanup` prunes).
    pub fn forget(&mut self, hashes: &[BlockHash]) {
        for h in hashes {
            self.data.remove(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> BlockHash {
        [n + 1; 32]
    }

    fn producer(n: u8) -> [u8; 32] {
        [0x10 * n; 32]
    }

    /// Build a DAG + GhostdagStore from `(hash, parents, height, producer)`
    /// specs, in the given order (must be a valid topological order — parents
    /// before children, as `Braid` guarantees upstream).
    fn build(
        specs: &[(BlockHash, Vec<BlockHash>, u64, [u8; 32])],
        k: u32,
    ) -> (BitfieldDag, GhostdagStore) {
        let mut dag = BitfieldDag::new();
        let mut store = GhostdagStore::new(k);
        for (hash, parents, height, prod) in specs {
            dag.add_vertex(*hash, parents, *height, *prod, 0);
            store.compute(&dag, *hash, parents);
        }
        (dag, store)
    }

    /// 32-byte hash from a u64 — the `h(n: u8)` helper tops out at 256 and
    /// cannot reach the index width where the 2026-08-28 bug lived.
    fn hw(n: u64) -> BlockHash {
        let mut out = [0u8; 32];
        out[..8].copy_from_slice(&(n + 1).to_le_bytes());
        out
    }

    /// Regression, 2026-08-28. A block must be a member of its own blue set.
    ///
    /// This is the minimal invariant whose silent violation disabled GHOSTDAG
    /// colouring for every chain past 64 blocks: `full_blue_set` was inherited
    /// by cloning the selected parent's, which preserved its width, and
    /// nothing ever grew it. `VertexBitfield::set` no-ops out of range, so
    /// from index 64 onward a block could not even record itself.
    #[test]
    fn every_block_is_in_its_own_blue_set_past_one_word() {
        let mut dag = BitfieldDag::new();
        let mut store = GhostdagStore::new(18);
        let n = 500u64; // comfortably past the 64-bit single-word width

        dag.add_vertex(hw(0), &[], 0, [1u8; 32], 0);
        store.compute(&dag, hw(0), &[]);
        for i in 1..=n {
            dag.add_vertex(hw(i), &[hw(i - 1)], i, [1u8; 32], 0);
            store.compute(&dag, hw(i), &[hw(i - 1)]);
        }

        for i in 0..=n {
            assert!(
                store.is_blue(&dag, &hw(i), &hw(i)),
                "block at index {i} is not in its own blue set"
            );
        }
    }

    /// Regression, 2026-08-28. A stored blue set must address the whole index
    /// space, or `test` silently answers `false` past its width and every
    /// query beyond it is indistinguishable from an honest "not blue".
    #[test]
    fn stored_blue_sets_stay_at_least_as_wide_as_the_index_space() {
        let mut dag = BitfieldDag::new();
        let mut store = GhostdagStore::new(18);
        let n = 500u64;

        dag.add_vertex(hw(0), &[], 0, [1u8; 32], 0);
        store.compute(&dag, hw(0), &[]);
        for i in 1..=n {
            dag.add_vertex(hw(i), &[hw(i - 1)], i, [1u8; 32], 0);
            store.compute(&dag, hw(i), &[hw(i - 1)]);
        }

        let tip = hw(n);
        let width = store.blue_set_of(&dag, &tip).width();
        assert!(
            width >= dag.capacity(),
            "tip blue set is {width} bits wide against an index space of {}",
            dag.capacity()
        );
        // And it really is populated, not merely wide: a linear chain colours
        // every block blue, so the count must track the chain length.
        assert_eq!(
            store.blue_set_of(&dag, &tip).count(),
            (n + 1) as usize,
            "a straight chain must have every block in the tip's blue set"
        );
    }

    /// The k-cluster admission test reads `blue_set.test(i)`. If the set is
    /// truncated it saturates at an all-ones 64-bit window, so admission stops
    /// consulting the real blue set. Guard the property admission depends on:
    /// membership answers must be correct for indices past one word.
    #[test]
    fn blue_membership_is_correct_past_one_word() {
        let mut dag = BitfieldDag::new();
        let mut store = GhostdagStore::new(18);
        let n = 300u64;

        dag.add_vertex(hw(0), &[], 0, [1u8; 32], 0);
        store.compute(&dag, hw(0), &[]);
        for i in 1..=n {
            dag.add_vertex(hw(i), &[hw(i - 1)], i, [1u8; 32], 0);
            store.compute(&dag, hw(i), &[hw(i - 1)]);
        }

        let tip = hw(n);
        // Every ancestor is blue relative to the tip...
        for i in 0..=n {
            assert!(store.is_blue(&dag, &tip, &hw(i)), "ancestor {i}");
        }
        // ...and a vertex that is not an ancestor is not blue. Add a sibling
        // hanging off an early block: it is concurrent with the tip's chain,
        // never merged, so it must not appear in the tip's blue set.
        let sibling = hw(n + 1_000);
        dag.add_vertex(sibling, &[hw(10)], 11, [2u8; 32], 0);
        store.compute(&dag, sibling, &[hw(10)]);
        assert!(
            !store.is_blue(&dag, &tip, &sibling),
            "an unmerged concurrent block must not be blue relative to the tip"
        );
    }

    // ── work weighting (2026-08-28) ─────────────────────────────────────────

    /// The load-bearing safety property: with the DEFAULT policy, `blue_work`
    /// is numerically identical to `blue_score` on every block — including on a
    /// DAG with real concurrency, reds and merges. `select_tip` was switched
    /// from comparing score to comparing work; this is what makes that switch
    /// provably not a consensus change.
    #[test]
    fn blue_work_equals_blue_score_under_uniform_count() {
        let mut dag = BitfieldDag::new();
        let mut store = GhostdagStore::new(2);
        assert_eq!(store.policy(), WorkPolicy::UniformCount, "default must be the no-op");

        // genesis
        dag.add_vertex(h(0), &[], 0, producer(0), 0);
        store.compute(&dag, h(0), &[]);
        // two concurrent children, then a merge — real width, not a chain
        dag.add_vertex(h(1), &[h(0)], 1, producer(1), 7);
        store.compute(&dag, h(1), &[h(0)]);
        dag.add_vertex(h(2), &[h(0)], 1, producer(2), 31);
        store.compute(&dag, h(2), &[h(0)]);
        dag.add_vertex(h(3), &[h(1), h(2)], 2, producer(3), 0);
        store.compute(&dag, h(3), &[h(1), h(2)]);

        for b in [h(0), h(1), h(2), h(3)] {
            assert_eq!(
                store.blue_work(&b) as u64,
                store.blue_score(&b),
                "block {:?}: work {} != score {} under UniformCount",
                &b[..1], store.blue_work(&b), store.blue_score(&b)
            );
        }
    }

    /// Differing `difficulty` must have NO effect while the default policy is
    /// in force — otherwise merely plumbing the field through would have
    /// changed selection.
    #[test]
    fn difficulty_is_inert_under_the_default_policy() {
        fn tip_with(diffs: [u64; 3]) -> (u128, u64) {
            let mut dag = BitfieldDag::new();
            let mut store = GhostdagStore::new(2);
            dag.add_vertex(h(0), &[], 0, producer(0), diffs[0]);
            store.compute(&dag, h(0), &[]);
            dag.add_vertex(h(1), &[h(0)], 1, producer(1), diffs[1]);
            store.compute(&dag, h(1), &[h(0)]);
            dag.add_vertex(h(2), &[h(1)], 2, producer(2), diffs[2]);
            store.compute(&dag, h(2), &[h(1)]);
            (store.blue_work(&h(2)), store.blue_score(&h(2)))
        }
        assert_eq!(tip_with([0, 0, 0]), tip_with([0, 40, 63]));
    }

    /// The weight function itself: an EXPONENT, not a linear count. This is the
    /// unit-confusion that would silently under-weight strong blocks if someone
    /// summed `bits` directly.
    #[test]
    fn exponential_policy_treats_difficulty_as_an_exponent() {
        let p = WorkPolicy::Exponential { base: 1 };
        assert_eq!(p.weight(10), 1024);
        assert_eq!(p.weight(11), 2048, "one more bit must DOUBLE the weight, not add 1");
        // saturates rather than shifting past the width of u128
        assert_eq!(p.weight(63), 1u128 << 63);
        assert_eq!(p.weight(9_999), 1u128 << 63);
    }

    /// A block that declares no work must never weigh zero: a zero-weight block
    /// is invisible to branch comparison, which is the exact failure this whole
    /// mechanism exists to prevent. On this chain 99.83% of blocks declare
    /// nothing, so this floor is what keeps `Exponential` survivable at all.
    #[test]
    fn undeclared_work_is_floored_never_zero() {
        for p in [WorkPolicy::UniformCount, WorkPolicy::Exponential { base: 1 },
                  WorkPolicy::Exponential { base: 8 }] {
            assert!(p.weight(0) >= 1, "{p:?} gave zero weight to an undeclared block");
        }
        assert_eq!(WorkPolicy::Exponential { base: 8 }.weight(0), 8);
        // and a declared-but-tiny difficulty never drops below the floor either
        assert_eq!(WorkPolicy::Exponential { base: 8 }.weight(1), 8);
    }

    /// Under `Exponential`, a branch built from fewer but much harder blocks
    /// outweighs a longer branch of easy ones. This is the hazard the comment
    /// on `should_produce()` describes, demonstrated as a behaviour change —
    /// and it is why the policy is opt-in and consensus-affecting.
    #[test]
    fn exponential_lets_harder_work_outweigh_more_blocks() {
        fn work_of_chain(policy: WorkPolicy, diffs: &[u64]) -> u128 {
            let mut dag = BitfieldDag::new();
            let mut store = GhostdagStore::with_policy(2, policy);
            dag.add_vertex(hw(0), &[], 0, producer(0), 0);
            store.compute(&dag, hw(0), &[]);
            for (i, d) in diffs.iter().enumerate() {
                let n = i as u64 + 1;
                dag.add_vertex(hw(n), &[hw(n - 1)], n, producer(1), *d);
                store.compute(&dag, hw(n), &[hw(n - 1)]);
            }
            store.blue_work(&hw(diffs.len() as u64))
        }
        let strong = work_of_chain(WorkPolicy::Exponential { base: 1 }, &[20, 20]);
        let weak   = work_of_chain(WorkPolicy::Exponential { base: 1 }, &[0; 50]);
        assert!(strong > weak, "2 hard blocks ({strong}) should outweigh 50 empty ones ({weak})");

        // ...and under the default the longer chain wins, exactly as today.
        let strong_u = work_of_chain(WorkPolicy::UniformCount, &[20, 20]);
        let weak_u   = work_of_chain(WorkPolicy::UniformCount, &[0; 50]);
        assert!(weak_u > strong_u, "UniformCount must still be a pure count");
    }

    /// `select_tip` picks the heavier branch, and under the default that is
    /// identical to picking the higher blue score.
    #[test]
    fn select_tip_matches_blue_score_under_uniform() {
        let mut dag = BitfieldDag::new();
        let mut store = GhostdagStore::new(2);
        dag.add_vertex(h(0), &[], 0, producer(0), 0);
        store.compute(&dag, h(0), &[]);
        dag.add_vertex(h(1), &[h(0)], 1, producer(1), 0);
        store.compute(&dag, h(1), &[h(0)]);
        dag.add_vertex(h(2), &[h(1)], 2, producer(2), 0);
        store.compute(&dag, h(2), &[h(1)]);

        let cands = [h(0), h(1), h(2)];
        let by_work = store.select_tip(cands.iter()).unwrap();
        let by_score = cands.iter().copied().reduce(|a, b| {
            let (sa, sb) = (store.blue_score(&a), store.blue_score(&b));
            if sb > sa || (sb == sa && b < a) { b } else { a }
        }).unwrap();
        assert_eq!(by_work, by_score);
    }

    #[test]
    fn genesis_has_zero_blue_score_and_no_selected_parent() {
        let (_, store) = build(&[(h(0), vec![], 0, producer(0))], 2);
        let d = store.get(&h(0)).unwrap();
        assert_eq!(d.selected_parent, None);
        assert_eq!(d.blue_score, 0);
        assert!(d.merge_set_blues.is_empty());
        assert!(d.merge_set_reds.is_empty());
    }

    #[test]
    fn linear_chain_blue_score_tracks_height() {
        let mut specs = vec![(h(0), vec![], 0, producer(0))];
        for i in 1u8..=5 {
            specs.push((h(i), vec![h(i - 1)], i as u64, producer(0)));
        }
        let (_, store) = build(&specs, 2);
        for i in 0u8..=5 {
            assert_eq!(store.blue_score(&h(i)), i as u64, "height {i}");
            let d = store.get(&h(i)).unwrap();
            assert!(d.merge_set_reds.is_empty(), "linear chain has no concurrency, no reds");
        }
    }

    #[test]
    fn two_way_concurrency_within_k_both_go_blue() {
        // genesis -> {A, B} concurrent, then C merges both.
        let g = h(0);
        let a = h(1);
        let b = h(2);
        let c = h(3);
        let specs = vec![
            (g, vec![], 0, producer(0)),
            (a, vec![g], 1, producer(1)),
            (b, vec![g], 1, producer(2)),
            (c, vec![a, b], 2, producer(1)),
        ];
        let (dag, store) = build(&specs, 2);
        let dc = store.get(&c).unwrap();
        // One of {a,b} is selected parent, the other must be in merge_set_blues
        // (anticone size 1 <= k=2, nothing else contends).
        assert_eq!(dc.merge_set_blues.len(), 1);
        assert!(dc.merge_set_reds.is_empty());
        assert_eq!(dc.blue_score, 3); // selected_parent(1) + 1 + 1 new blue
        assert!(store.is_blue(&dag, &c, &a));
        assert!(store.is_blue(&dag, &c, &b));
    }

    #[test]
    fn k_cluster_bound_rejects_excess_concurrency() {
        // genesis -> 5 concurrent siblings (P0..P4) -> B merges all 5
        // (spine + 4 merge parents). k=2: exactly 2 siblings besides the
        // selected parent can be absorbed blue; the rest must go red.
        let g = h(0);
        let siblings: Vec<BlockHash> = (1u8..=5).map(h).collect();
        let mut specs = vec![(g, vec![], 0, producer(0))];
        for (i, s) in siblings.iter().enumerate() {
            specs.push((*s, vec![g], 1, producer(i as u8 + 1)));
        }
        let b = h(6);
        // spine = siblings[0], merge_parents = the other 4.
        let parents: Vec<BlockHash> = siblings.clone();
        specs.push((b, parents, 2, producer(6)));

        let (dag, store) = build(&specs, 2);
        let db = store.get(&b).unwrap();

        // All 5 siblings accounted for: 1 implicit (selected parent) + blues + reds = 5.
        assert_eq!(1 + db.merge_set_blues.len() + db.merge_set_reds.len(), 5);
        // k=2 means the selected parent can absorb at most 2 more into blue
        // before a 4th concurrent sibling would push some blue block's
        // blue-anticone past k. Expect exactly 2 blues, 2 reds.
        assert_eq!(db.merge_set_blues.len(), 2, "blues: {:?}", db.merge_set_blues);
        assert_eq!(db.merge_set_reds.len(), 2, "reds: {:?}", db.merge_set_reds);
        assert_eq!(db.blue_score, 4); // 1 (selected parent's own score) + 1 (it enters) + 2 new

        // K-CLUSTER INVARIANT: every blue block's anticone, restricted to the
        // final blue set, has size <= k.
        let blue_set = store.blue_set_of(&dag, &b);
        for blue_idx in blue_set.iter_set_bits() {
            let blue = dag.hash_of(blue_idx).expect("set bit must be a resident vertex");
            let ac = dag.anticone_hashes(&blue).unwrap_or_default();
            let blue_ac = ac
                .iter()
                .filter(|x| dag.index_of(x).is_some_and(|i| blue_set.test(i)))
                .count();
            assert!(blue_ac as u32 <= 2, "blue {blue:?} has blue-anticone {blue_ac} > k=2");
        }
        // And every red is red BECAUSE admitting it would have broken the bound
        // (sanity: reds are not in the final blue set).
        for red in &db.merge_set_reds {
            assert!(!dag.index_of(red).is_some_and(|i| blue_set.test(i)));
        }
    }

    #[test]
    fn selected_parent_is_highest_blue_score_not_first_listed() {
        // Give one of two same-height parents a head start in blue score by
        // routing extra (non-competing) history through it first.
        let g = h(0);
        let a = h(1); // will get ahead
        let b = h(2);
        let a2 = h(3); // extends A's lineage alone before the merge
        let specs = vec![
            (g, vec![], 0, producer(0)),
            (a, vec![g], 1, producer(1)),
            (b, vec![g], 1, producer(2)),
            (a2, vec![a], 2, producer(1)), // A's blue_score becomes 2
        ];
        let (dag, mut store) = build(&specs, 4);
        assert_eq!(store.blue_score(&a2), 2);
        assert_eq!(store.blue_score(&b), 1);

        let c = h(4);
        let parents = vec![a2, b];
        // Manually mirror what Braid does: add to dag then compute.
        let mut dag = dag;
        dag.add_vertex(c, &parents, 3, producer(3), 0);
        let dc = store.compute(&dag, c, &parents);
        assert_eq!(dc.selected_parent, Some(a2), "higher blue score must win selection");
    }

    #[test]
    fn coloring_is_order_invariant_across_two_valid_insertion_orders() {
        // A modest random-ish braid (deterministic generator), fed to two
        // independent stores in different valid topological orders. Final
        // blue_score for every block must match — the coloring is a pure
        // function of the DAG, not of arrival order (mirrors the crate's own
        // frontier-determinism tests for v1).
        struct XorShift(u64);
        impl XorShift {
            fn next(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.0 = x;
                x
            }
        }
        let mut rng = XorShift(0xC0FFEE ^ 1);
        let producers: Vec<[u8; 32]> = (0u8..4).map(producer).collect();
        let mut specs: Vec<(BlockHash, Vec<BlockHash>, u64, [u8; 32])> =
            vec![(h(0), vec![], 0, producers[0])];
        let mut tips: Vec<BlockHash> = vec![h(0)];
        let mut heights: HashMap<BlockHash, u64> = HashMap::new();
        heights.insert(h(0), 0);
        for n in 1u8..40 {
            let prod = producers[(rng.next() as usize) % producers.len()];
            let spine = tips[(rng.next() as usize) % tips.len()];
            let spine_h = heights[&spine];
            let mut parents = vec![spine];
            if rng.next() % 2 == 0 && specs.len() > 1 {
                let mp = specs[(rng.next() as usize) % specs.len()].0;
                if mp != spine {
                    parents.push(mp);
                }
            }
            let hash = h(n);
            let height = spine_h + 1;
            specs.push((hash, parents.clone(), height, prod));
            heights.insert(hash, height);
            tips.retain(|t| !parents.contains(t));
            tips.push(hash);
        }

        let (_, reference) = build(&specs, 3);

        // A different valid order: reverse-stable topological shuffle via
        // repeated ready-set random pick.
        let by_hash: HashMap<BlockHash, &(BlockHash, Vec<BlockHash>, u64, [u8; 32])> =
            specs.iter().map(|s| (s.0, s)).collect();
        let mut emitted: HashSet<BlockHash> = HashSet::new();
        let mut remaining: Vec<&(BlockHash, Vec<BlockHash>, u64, [u8; 32])> = specs.iter().collect();
        let mut shuffled = Vec::with_capacity(specs.len());
        let mut rng2 = XorShift(0xC0FFEE ^ 777);
        while !remaining.is_empty() {
            let ready: Vec<usize> = remaining
                .iter()
                .enumerate()
                .filter(|(_, s)| s.1.iter().all(|p| emitted.contains(p) || !by_hash.contains_key(p)))
                .map(|(i, _)| i)
                .collect();
            let pick = ready[(rng2.next() as usize) % ready.len()];
            let spec = remaining.remove(pick);
            emitted.insert(spec.0);
            shuffled.push(spec.clone());
        }

        let (_, other) = build(&shuffled, 3);

        for (hash, ..) in &specs {
            assert_eq!(
                reference.blue_score(hash),
                other.blue_score(hash),
                "blue_score diverged for {hash:?}"
            );
        }
    }
}
