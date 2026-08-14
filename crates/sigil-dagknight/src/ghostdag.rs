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
//! Also not (yet) included: blue **work** (difficulty-weighted) as the
//! selection/finality metric — this module uses blue **score** (a count),
//! which is the right metric only when block-minting difficulty is roughly
//! uniform across producers. `Braid`'s finality window (`final_depth`)
//! likewise still counts in **height**, not blue score — see
//! `Braid::computed_final`. Both are explicit, scoped follow-ups, not silent
//! gaps.
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
use std::collections::HashSet;

use sigil_header::BlockHash;

use crate::bitset::BitfieldDag;

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
}

/// Incremental GHOSTDAG blue/red coloring store, parameterized by the fixed
/// cluster bound `k`. Lives alongside [`crate::braid::Braid`]'s window; the
/// caller (`Braid`) is responsible for calling [`GhostdagStore::forget`] in
/// lockstep with its own window cleanup so the two stay bounded together.
pub struct GhostdagStore {
    k: u32,
    data: HashMap<BlockHash, BlockGhostdagData>,
}

impl GhostdagStore {
    /// New empty store for cluster parameter `k`.
    pub fn new(k: u32) -> Self {
        Self {
            k,
            data: HashMap::new(),
        }
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

    /// Reconstruct the full blue set of `block`: itself, plus every ancestor
    /// reachable by walking `selected_parent` pointers within the resident
    /// window, plus each of those ancestors' local `merge_set_blues`. See
    /// the module doc's complexity note.
    fn blue_set_of(&self, block: &BlockHash) -> HashSet<BlockHash> {
        let mut set = HashSet::new();
        let mut cur = Some(*block);
        while let Some(h) = cur {
            if !set.insert(h) {
                break; // defensive: a cycle should be structurally impossible
            }
            let Some(d) = self.data.get(&h) else { break };
            for b in &d.merge_set_blues {
                set.insert(*b);
            }
            cur = d.selected_parent;
        }
        set
    }

    /// True iff `target` is blue relative to `tip` (on `tip`'s
    /// selected-parent chain, or in some chain ancestor's `merge_set_blues`).
    pub fn is_blue(&self, tip: &BlockHash, target: &BlockHash) -> bool {
        self.blue_set_of(tip).contains(target)
    }

    /// Select the candidate with the highest blue score, tie-broken by the
    /// smaller hash — the v2 analog of `Braid`'s v1 max-height/min-hash tip
    /// selection.
    pub fn select_tip<'a>(&self, candidates: impl Iterator<Item = &'a BlockHash>) -> Option<BlockHash> {
        candidates.copied().reduce(|a, b| {
            let sa = self.blue_score(&a);
            let sb = self.blue_score(&b);
            if sb > sa || (sb == sa && b < a) {
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
            self.data.insert(
                block,
                BlockGhostdagData {
                    selected_parent: None,
                    merge_set_blues: Vec::new(),
                    merge_set_reds: Vec::new(),
                    blue_score: 0,
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

        // 4. Greedy k-cluster admission.
        let mut blue_set = self.blue_set_of(&selected_parent);
        let mut new_blues = Vec::new();
        let mut new_reds = Vec::new();
        for cand in merge_set {
            let cand_anticone = dag.anticone_hashes(&cand).unwrap_or_default();
            let blue_anticone: Vec<BlockHash> = cand_anticone
                .iter()
                .copied()
                .filter(|h| blue_set.contains(h))
                .collect();

            let admits = blue_anticone.len() as u32 <= self.k
                && blue_anticone.iter().all(|anc| {
                    let anc_anticone = dag.anticone_hashes(anc).unwrap_or_default();
                    let anc_blue_count = anc_anticone
                        .iter()
                        .filter(|h| blue_set.contains(*h))
                        .count() as u32;
                    // +1 for `cand` itself joining anc's blue anticone
                    // (anticone-symmetry: cand ∈ anticone(anc) here).
                    anc_blue_count + 1 <= self.k
                });

            if admits {
                blue_set.insert(cand);
                new_blues.push(cand);
            } else {
                new_reds.push(cand);
            }
        }

        let blue_score = self.blue_score(&selected_parent) + 1 + new_blues.len() as u64;

        self.data.insert(
            block,
            BlockGhostdagData {
                selected_parent: Some(selected_parent),
                merge_set_blues: new_blues,
                merge_set_reds: new_reds,
                blue_score,
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
            dag.add_vertex(*hash, parents, *height, *prod);
            store.compute(&dag, *hash, parents);
        }
        (dag, store)
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
        let (_, store) = build(&specs, 2);
        let dc = store.get(&c).unwrap();
        // One of {a,b} is selected parent, the other must be in merge_set_blues
        // (anticone size 1 <= k=2, nothing else contends).
        assert_eq!(dc.merge_set_blues.len(), 1);
        assert!(dc.merge_set_reds.is_empty());
        assert_eq!(dc.blue_score, 3); // selected_parent(1) + 1 + 1 new blue
        assert!(store.is_blue(&c, &a));
        assert!(store.is_blue(&c, &b));
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
        let blue_set = store.blue_set_of(&b);
        for blue in &blue_set {
            let ac = dag.anticone_hashes(blue).unwrap_or_default();
            let blue_ac = ac.iter().filter(|x| blue_set.contains(*x)).count();
            assert!(blue_ac as u32 <= 2, "blue {blue:?} has blue-anticone {blue_ac} > k=2");
        }
        // And every red is red BECAUSE admitting it would have broken the bound
        // (sanity: reds are not in the final blue set).
        for red in &db.merge_set_reds {
            assert!(!blue_set.contains(red));
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
        dag.add_vertex(c, &parents, 3, producer(3));
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
