//! The braid ordering core — **deterministic braid linearization** over the
//! committed `parent_hash` / `merge_parents` block DAG.
//!
//! NOT GHOSTDAG, NOT DagKnight-the-paper (see the crate header and
//! `docs/SIGIL_DAGKNIGHT_LANE_v0.md` §1). This module implements the v1 rule:
//!
//! 1. Kahn topological sort with a `BTreeSet` ready-frontier keyed
//!    `(height, producer, hash)` — emit the minimum; a block is *ready* only
//!    when ALL of `{parent_hash} ∪ merge_parents` have already been emitted.
//! 2. Selected spine = `parent_hash` walk back from the selected tip
//!    (max height, min-hash tie-break — grindable, documented v1 limitation).
//! 3. Finality window: once the selected tip is ≥ `final_depth` above height
//!    *h*, the linearized prefix through *h* is frozen; inserts at height ≤
//!    finalized height are refused (`InsertOutcome::BelowFinal`) — the DAG
//!    analog of the sync-down guard.
//! 4. `order_hash` = chained BLAKE3 over the linearized hashes.
//!
//! Incremental-emission stability (why `drain_ordered` is finality-gated):
//! the frontier key leads with height, and every *future* legal insert has
//! height > finalized (the BelowFinal guard), so a ready frontier-minimum at
//! height ≤ finalized can never be preceded by any later arrival in a batch
//! re-linearization — its position is immutable. Blocks above the finality
//! line stay fluid and are only visible through the batch [`Braid::linearize`].
//! Hence the invariant: `concat(all drains)` == a prefix of `linearize()`,
//! with equality over every block once the tip has advanced `final_depth`
//! past it.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use sigil_header::BlockHash;

use crate::bitset::BitfieldDag;
use crate::ghostdag::GhostdagStore;
use crate::view::BlockView;
use crate::{BraidConfig, InsertOutcome};

/// Deterministic ordering key: `(height, producer, hash)`.
type Key = (u64, [u8; 32], BlockHash);

/// Per-block window record.
struct BlockRec {
    view: BlockView,
    /// Number of parents (spine + merge) not yet emitted. 0 ⇒ ready.
    deps_unmet: usize,
    /// Emitted into the frozen order (still resident until cleanup).
    emitted: bool,
}

impl BlockRec {
    fn key(&self) -> Key {
        (self.view.height, self.view.producer, self.view.hash)
    }
}

/// Result of attempting to move a view into the active window.
enum Accept {
    Ok,
    Missing(Vec<BlockHash>),
    Bad(&'static str),
    WindowFull,
}

/// The braid: incremental deterministic linearization of the block DAG.
///
/// Composition: the lane-A [`BitfieldDag`] substrate (edges, transitive
/// causality, hard sliding window) + a ready-frontier + a parked/pending set
/// + the frozen (finalized) prefix. See the module doc for the ordering rule.
pub struct Braid {
    cfg: BraidConfig,
    /// Lane-A substrate: kept in lock-step with `recs` (same membership, same
    /// cleanup cutoff). Provides O(1) causality + anticone for the v2 lane.
    dag: BitfieldDag,
    /// v2 GHOSTDAG blue/red coloring store. `None` unless
    /// `cfg.ghostdag_k.is_some()` — v1 behavior is completely unaffected
    /// when this is `None` (see the `ghostdag` module doc).
    ghostdag: Option<GhostdagStore>,
    /// Active window records, keyed by block hash.
    recs: HashMap<BlockHash, BlockRec>,
    /// known-parent → children within the window (spine + merge edges).
    children: HashMap<BlockHash, Vec<BlockHash>>,
    /// Ready (all parents emitted) and not yet emitted.
    frontier: BTreeSet<Key>,
    /// Parked views waiting on unknown parents.
    pending: HashMap<BlockHash, BlockView>,
    /// Height → count over `pending`, so the lowest parked height is O(log n)
    /// instead of an O(pending) scan on every `computed_final()` (which runs
    /// per insert). Kept in lock-step with `pending` by `park`/`unpark_take`.
    pending_heights: BTreeMap<u64, usize>,
    /// missing-parent hash → parked children awaiting it.
    waiters: HashMap<BlockHash, Vec<BlockHash>>,
    /// Emitted hash → height. Retention-pruned to `max_window` heights below
    /// the finalized height; merge parents older than that are treated as
    /// unknown (parked → pending-capped) — bounded-memory honesty.
    emitted_at: HashMap<BlockHash, u64>,
    /// The frozen (finalized) linear order, append-only.
    frozen: Vec<BlockHash>,
    /// Running chained-BLAKE3 accumulator over `frozen`.
    frozen_acc: [u8; 32],
    /// How many of `frozen` have been handed out by `drain_ordered`.
    drained: usize,
    below_final_count: u64,
    rejected_count: u64,
    /// Parked views dropped at unpark time (turned stale/invalid/window-full).
    dropped_count: u64,
}

fn chain_hash(acc: &[u8; 32], h: &BlockHash) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(acc);
    hasher.update(h);
    *hasher.finalize().as_bytes()
}

impl Braid {
    /// New empty braid. Seed with the genesis view (height 0) first — blocks
    /// arriving before their ancestors are parked until backfill supplies
    /// them.
    pub fn new(cfg: BraidConfig) -> Self {
        let ghostdag = cfg.ghostdag_k.map(GhostdagStore::new);
        Self {
            cfg,
            dag: BitfieldDag::new(),
            ghostdag,
            recs: HashMap::new(),
            children: HashMap::new(),
            frontier: BTreeSet::new(),
            pending: HashMap::new(),
            pending_heights: BTreeMap::new(),
            waiters: HashMap::new(),
            emitted_at: HashMap::new(),
            frozen: Vec::new(),
            frozen_acc: [0u8; 32],
            drained: 0,
            below_final_count: 0,
            rejected_count: 0,
            dropped_count: 0,
        }
    }

    /// New braid anchored at a trusted base block — for seeding from a PRUNED
    /// local window, where the base's own ancestry is pre-window and
    /// intentionally unknown. The base is recorded as already-emitted at its
    /// height so descendants chain normally, but it is NOT part of the
    /// emission order (never drained, absent from `order_hash`). Trust is the
    /// caller's responsibility: the base must be a block the node itself
    /// applied through the state chokepoint.
    pub fn new_with_base(cfg: BraidConfig, base_hash: BlockHash, base_height: u64) -> Self {
        let mut b = Self::new(cfg);
        b.anchor_trusted(base_hash, base_height);
        b
    }

    /// Record `hash` as trusted, already-emitted pre-frontier history at
    /// `height` (same trust model as [`Braid::new_with_base`] — the caller
    /// vouches, typically because the reference is committed inside a header
    /// this node already applied through the state chokepoint). Used when
    /// seeding from a window whose blocks carry merge edges to bodies that
    /// were never stored locally (e.g. the other strand, pre-catch-up).
    /// No-op if the hash is already known to the braid; never enters the
    /// emission order.
    pub fn anchor_trusted(&mut self, hash: BlockHash, height: u64) {
        if self.recs.contains_key(&hash) || self.emitted_at.contains_key(&hash) {
            return;
        }
        self.emitted_at.insert(hash, height);
    }

    /// Insert a block view. Every input yields a structured outcome — never
    /// panics on foreign data. `newly_ready` counts views (this one plus any
    /// unparked pendings) that entered the ordering window as a result.
    pub fn insert(&mut self, view: BlockView) -> InsertOutcome {
        if self.recs.contains_key(&view.hash) || self.pending.contains_key(&view.hash) {
            return InsertOutcome::Duplicate;
        }
        if let Some(reason) = self.structural_reject(&view) {
            self.rejected_count += 1;
            return InsertOutcome::Rejected(reason);
        }
        if let Some(f) = self.computed_final() {
            if view.height <= f {
                self.below_final_count += 1;
                return InsertOutcome::BelowFinal { finalized: f };
            }
        }
        match self.try_accept(&view) {
            Accept::Ok => {
                let hash = view.hash;
                let newly_ready = 1 + self.cascade_unpark(hash);
                InsertOutcome::Inserted { newly_ready }
            }
            Accept::Missing(missing) => {
                if self.pending.len() >= self.cfg.max_pending {
                    self.rejected_count += 1;
                    return InsertOutcome::Rejected("pending overflow");
                }
                for p in &missing {
                    self.waiters.entry(*p).or_default().push(view.hash);
                }
                self.park(view);
                InsertOutcome::MissingParents(missing)
            }
            Accept::Bad(reason) => {
                self.rejected_count += 1;
                InsertOutcome::Rejected(reason)
            }
            Accept::WindowFull => {
                self.rejected_count += 1;
                InsertOutcome::Rejected("window overflow")
            }
        }
    }

    /// Stateless structural validation.
    fn structural_reject(&self, view: &BlockView) -> Option<&'static str> {
        if view.parent == view.hash {
            return Some("self-parent");
        }
        if view.merge_parents.len() > self.cfg.max_merge_parents {
            return Some("too many merge parents");
        }
        if view.height == 0 && !view.merge_parents.is_empty() {
            return Some("root block with merge parents");
        }
        for (i, mp) in view.merge_parents.iter().enumerate() {
            if *mp == view.hash {
                return Some("self merge parent");
            }
            if view.height > 0 && *mp == view.parent {
                return Some("merge parent duplicates spine parent");
            }
            if view.merge_parents[..i].contains(mp) {
                return Some("duplicate merge parent");
            }
        }
        None
    }

    /// Height of a known (window or recently-emitted) block.
    fn known_height(&self, h: &BlockHash) -> Option<u64> {
        self.recs
            .get(h)
            .map(|r| r.view.height)
            .or_else(|| self.emitted_at.get(h).copied())
    }

    /// Try to move a view into the active window. Requires all parents known.
    /// Park a view, keeping the `pending_heights` index in lock-step.
    fn park(&mut self, view: BlockView) {
        let h = view.height;
        if self.pending.insert(view.hash, view).is_none() {
            *self.pending_heights.entry(h).or_insert(0) += 1;
        }
    }

    /// Take a view out of the parked set, keeping the index in lock-step.
    fn unpark_take(&mut self, hash: &BlockHash) -> Option<BlockView> {
        let view = self.pending.remove(hash)?;
        if let std::collections::btree_map::Entry::Occupied(mut e) =
            self.pending_heights.entry(view.height)
        {
            *e.get_mut() -= 1;
            if *e.get() == 0 {
                e.remove();
            }
        }
        Some(view)
    }

    /// Lowest height currently parked, if any.
    fn pending_floor(&self) -> Option<u64> {
        self.pending_heights.keys().next().copied()
    }

    /// Recompute the height index from `pending` (used after a bulk `retain`).
    fn rebuild_pending_heights(&mut self) {
        self.pending_heights.clear();
        for v in self.pending.values() {
            *self.pending_heights.entry(v.height).or_insert(0) += 1;
        }
    }

    fn try_accept(&mut self, view: &BlockView) -> Accept {
        let mut parents: Vec<BlockHash> = Vec::new();
        if view.height > 0 {
            parents.push(view.parent);
            parents.extend(view.merge_parents.iter().copied());

            let missing: Vec<BlockHash> = parents
                .iter()
                .filter(|p| self.known_height(p).is_none())
                .copied()
                .collect();
            if !missing.is_empty() {
                return Accept::Missing(missing);
            }
            // Spine parent height must be exactly ours − 1. Merge parents are
            // unconstrained in height (live tips can sit above the child).
            let ph = self.known_height(&view.parent).unwrap_or(0);
            if ph + 1 != view.height {
                return Accept::Bad("height not parent height + 1");
            }
        }
        if self.recs.len() >= self.cfg.max_window {
            return Accept::WindowFull;
        }

        // Substrate edges: only parents still resident in the window (emitted
        // + cleaned parents are the finalized past — satisfied by definition).
        let dag_parents: Vec<BlockHash> = parents
            .iter()
            .filter(|p| self.recs.contains_key(*p))
            .copied()
            .collect();
        self.dag
            .add_vertex(view.hash, &dag_parents, view.height, view.producer);
        if let Some(store) = &mut self.ghostdag {
            store.compute(&self.dag, view.hash, &dag_parents);
        }

        let mut deps_unmet = 0usize;
        for p in &dag_parents {
            self.children.entry(*p).or_default().push(view.hash);
            if !self.recs[p].emitted {
                deps_unmet += 1;
            }
        }
        let rec = BlockRec {
            view: view.clone(),
            deps_unmet,
            emitted: false,
        };
        if deps_unmet == 0 {
            self.frontier.insert(rec.key());
        }
        self.recs.insert(view.hash, rec);
        Accept::Ok
    }

    /// After `accepted` entered the window, unpark every pending descendant
    /// that became insertable. Returns how many entered the window.
    fn cascade_unpark(&mut self, accepted: BlockHash) -> usize {
        let mut count = 0usize;
        let mut work: VecDeque<BlockHash> = VecDeque::new();
        if let Some(kids) = self.waiters.remove(&accepted) {
            work.extend(kids);
        }
        while let Some(child) = work.pop_front() {
            let Some(view) = self.unpark_take(&child) else {
                continue; // stale waiter entry
            };
            if let Some(f) = self.computed_final() {
                if view.height <= f {
                    self.below_final_count += 1;
                    continue;
                }
            }
            match self.try_accept(&view) {
                Accept::Ok => {
                    count += 1;
                    if let Some(kids) = self.waiters.remove(&view.hash) {
                        work.extend(kids);
                    }
                }
                Accept::Missing(missing) => {
                    // Re-park on the still-unknown parents.
                    for p in &missing {
                        self.waiters.entry(*p).or_default().push(view.hash);
                    }
                    self.park(view);
                }
                Accept::Bad(_) | Accept::WindowFull => {
                    // Can't surface an outcome to the original caller anymore.
                    self.dropped_count += 1;
                }
            }
        }
        count
    }

    /// Finalized height as a hard `Option`: `None` until the selected tip has
    /// cleared `final_depth`.
    ///
    /// **Parked-set clamp (deadlock fix).** The tip-derived line alone is
    /// computed only from RESIDENT records, so under out-of-order delivery a
    /// complete high chain can drag finality past heights whose blocks are
    /// still parked — and `cleanup` then destroys those parked views
    /// permanently (they can never be re-inserted: `insert` refuses them
    /// `BelowFinal`). Everything downstream of them is orphaned, the node
    /// stalls with a large pending set, and its worklist cannot recover it.
    /// Measured: P=6, k=1 stalled at 421/2400 blocks ordered.
    ///
    /// A height with a block still parked is by definition NOT ordered, so it
    /// must not be frozen. We therefore hold the line strictly below the
    /// lowest parked height.
    ///
    /// **Bounded, so it cannot be weaponised:** a peer that parks one bogus
    /// low-height view must not stall finality forever. The clamp may never
    /// pull the line more than `max_window` heights below the tip — past that
    /// the parked view is outside the retention band anyway and is dropped by
    /// the existing `cleanup` path.
    fn computed_final(&self) -> Option<u64> {
        let tip = self.selected_tip()?;
        let tip_height = self.recs.get(&tip)?.view.height;
        let tip_line = tip_height.checked_sub(self.cfg.final_depth)?;
        let Some(floor) = self.pending_floor() else {
            return Some(tip_line);
        };
        let clamped = tip_line.min(floor.saturating_sub(1));
        let hard_floor = tip_height.saturating_sub(self.cfg.max_window as u64);
        Some(clamped.max(hard_floor).min(tip_line))
    }

    /// Selected tip key: max height, min-hash tie-break, over the window.
    fn selected_tip_key(&self) -> Option<Key> {
        let mut best: Option<Key> = None;
        for rec in self.recs.values() {
            let k = rec.key();
            best = Some(match best {
                None => k,
                Some(b) => {
                    if k.0 > b.0 || (k.0 == b.0 && k.2 < b.2) {
                        k
                    } else {
                        b
                    }
                }
            });
        }
        best
    }

    /// Selected tip. v1 (no ghostdag): max height, min-hash tie-break. v2
    /// (`cfg.ghostdag_k` set): max blue score, min-hash tie-break — see the
    /// `ghostdag` module doc.
    pub fn selected_tip(&self) -> Option<BlockHash> {
        if let Some(store) = &self.ghostdag {
            store.select_tip(self.recs.keys())
        } else {
            self.selected_tip_key().map(|k| k.2)
        }
    }

    /// True iff `h` lies on the selected chain back from the selected tip
    /// (within the resident window — blocks cleaned below the retention band
    /// report `false`). v1 walks `view.parent`; v2 walks the GHOSTDAG
    /// `selected_parent` pointer instead — the chain that actually reflects
    /// blue-score-based selection.
    pub fn is_on_spine(&self, h: &BlockHash) -> bool {
        let Some(target) = self.recs.get(h) else {
            return false;
        };
        let target_height = target.view.height;
        let Some(tip) = self.selected_tip() else {
            return false;
        };
        if let Some(store) = &self.ghostdag {
            let mut cur = tip;
            loop {
                if cur == *h {
                    return true;
                }
                let Some(cur_height) = self
                    .recs
                    .get(&cur)
                    .map(|r| r.view.height)
                    .or_else(|| self.known_height(&cur))
                else {
                    return false;
                };
                if cur_height <= target_height {
                    return false;
                }
                let Some(data) = store.get(&cur) else {
                    return false;
                };
                let Some(next) = data.selected_parent else {
                    return false;
                };
                cur = next;
            }
        }
        let mut cur = tip;
        loop {
            if cur == *h {
                return true;
            }
            let Some(rec) = self.recs.get(&cur) else {
                return false;
            };
            if rec.view.height <= target_height {
                return false;
            }
            cur = rec.view.parent;
        }
    }

    /// v2 blue score of `h` (0 in v1 mode, or if `h` is unknown to the
    /// ghostdag store).
    pub fn blue_score(&self, h: &BlockHash) -> u64 {
        self.ghostdag.as_ref().map(|s| s.blue_score(h)).unwrap_or(0)
    }

    /// True iff v2 GHOSTDAG coloring is active for this braid
    /// (`cfg.ghostdag_k.is_some()`).
    pub fn is_ghostdag_active(&self) -> bool {
        self.ghostdag.is_some()
    }

    /// v2 only: true iff `h` is colored BLUE relative to the current selected
    /// tip. Always `false` in v1 mode (no coloring exists to query).
    pub fn is_blue(&self, h: &BlockHash) -> bool {
        let Some(store) = &self.ghostdag else {
            return false;
        };
        let Some(tip) = self.selected_tip() else {
            return false;
        };
        store.is_blue(&tip, h)
    }

    /// Current DAG tips (window blocks with no known children), minus
    /// `exclude` — the producer's merge-parent source. Deterministic order:
    /// height desc, hash asc; capped at `cap`.
    pub fn merge_tips(&self, exclude: &BlockHash, cap: usize) -> Vec<BlockHash> {
        let mut tips: Vec<(u64, BlockHash)> = self
            .recs
            .values()
            .filter(|r| {
                r.view.hash != *exclude
                    && self
                        .children
                        .get(&r.view.hash)
                        .is_none_or(|kids| kids.is_empty())
            })
            .map(|r| (r.view.height, r.view.hash))
            .collect();
        tips.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        tips.truncate(cap);
        tips.into_iter().map(|(_, h)| h).collect()
    }

    /// Deterministic Kahn order of the not-yet-emitted window: BTreeSet
    /// frontier keyed `(height, producer, hash)`, min first, parents (spine +
    /// merge) always before children.
    fn suffix_order(&self) -> Vec<BlockHash> {
        let mut indeg: HashMap<BlockHash, usize> = self
            .recs
            .values()
            .filter(|r| !r.emitted)
            .map(|r| (r.view.hash, r.deps_unmet))
            .collect();
        let mut frontier = self.frontier.clone();
        let mut out = Vec::with_capacity(indeg.len());
        while let Some(key) = frontier.pop_first() {
            let (_, _, hash) = key;
            out.push(hash);
            if let Some(kids) = self.children.get(&hash) {
                for kid in kids {
                    if let Some(d) = indeg.get_mut(kid) {
                        *d = d.saturating_sub(1);
                        if *d == 0 {
                            frontier.insert(self.recs[kid].key());
                        }
                    }
                }
            }
        }
        out
    }

    /// BATCH: the full deterministic linearization — the frozen (finalized)
    /// prefix followed by the Kahn order of the non-finalized window. A pure
    /// function of the DAG contents, arrival-order invariant.
    pub fn linearize(&self) -> Vec<BlockHash> {
        let mut out = self.frozen.clone();
        out.extend(self.suffix_order());
        out
    }

    /// INCREMENTAL: newly stable-ordered (finalized) blocks since the last
    /// drain. `concat(all drains)` is always a prefix of [`Braid::linearize`]
    /// over the same DAG — equality over every block once the selected tip is
    /// `final_depth` past it. See the module doc for the stability argument.
    pub fn drain_ordered(&mut self) -> Vec<BlockHash> {
        if let Some(f) = self.computed_final() {
            // Cleanup FIRST so everything emitted by this call stays resident
            // (spine-checkable by the caller) until the next drain.
            self.cleanup(f);
            while let Some(&key) = self.frontier.first() {
                if key.0 > f {
                    break;
                }
                self.frontier.pop_first();
                self.emit(key);
            }
        }
        let out = self.frozen[self.drained..].to_vec();
        self.drained = self.frozen.len();
        out
    }

    fn emit(&mut self, key: Key) {
        let (height, _, hash) = key;
        if let Some(rec) = self.recs.get_mut(&hash) {
            rec.emitted = true;
        }
        self.frozen.push(hash);
        self.frozen_acc = chain_hash(&self.frozen_acc, &hash);
        self.emitted_at.insert(hash, height);
        if let Some(kids) = self.children.get(&hash).cloned() {
            for kid in kids {
                if let Some(rec) = self.recs.get_mut(&kid) {
                    if !rec.emitted && rec.deps_unmet > 0 {
                        rec.deps_unmet -= 1;
                        if rec.deps_unmet == 0 {
                            self.frontier.insert(rec.key());
                        }
                    }
                }
            }
        }
    }

    /// Slide the hard window: drop emitted records more than `final_depth`
    /// below the finalized height (the retention band keeps freshly finalized
    /// blocks spine-checkable), prune stale pendings, bound `emitted_at`.
    fn cleanup(&mut self, f: u64) {
        let keep_from = f.saturating_sub(self.cfg.final_depth);
        let min_unemitted = self
            .recs
            .values()
            .filter(|r| !r.emitted)
            .map(|r| r.view.height)
            .min();
        let cutoff = min_unemitted.map_or(keep_from, |m| keep_from.min(m));
        if cutoff > 0 {
            self.dag.cleanup_below_height(cutoff);
            if let Some(store) = &mut self.ghostdag {
                let removed: Vec<BlockHash> = self
                    .recs
                    .iter()
                    .filter(|(_, r)| r.view.height < cutoff)
                    .map(|(h, _)| *h)
                    .collect();
                store.forget(&removed);
            }
            self.recs.retain(|_, r| r.view.height >= cutoff);
            self.children.retain(|p, _| self.recs.contains_key(p));
            for kids in self.children.values_mut() {
                kids.retain(|k| self.recs.contains_key(k));
            }
        }
        // Pendings at or below the finality line can never be accepted.
        let before = self.pending.len();
        self.pending.retain(|_, v| v.height > f);
        if self.pending.len() != before {
            self.rebuild_pending_heights();
        }
        self.below_final_count += (before - self.pending.len()) as u64;
        for kids in self.waiters.values_mut() {
            kids.retain(|k| self.pending.contains_key(k));
        }
        self.waiters.retain(|_, kids| !kids.is_empty());
        // Bound the emitted-hash memory (merge parents deeper than
        // max_window heights below final are treated as unknown).
        let emitted_cutoff = f.saturating_sub(self.cfg.max_window as u64);
        if emitted_cutoff > 0 {
            self.emitted_at.retain(|_, h| *h >= emitted_cutoff);
        }
    }

    /// Finalized height — 0 until the selected tip has cleared `final_depth`
    /// (indistinguishable from "finalized through genesis"; the `BelowFinal`
    /// guard internally distinguishes the two).
    pub fn finalized_height(&self) -> u64 {
        self.computed_final().unwrap_or(0)
    }

    /// Chained BLAKE3 over the full linearized order (`acc = BLAKE3(acc ‖
    /// block_hash)` from a zero accumulator) — the one-word divergence
    /// detector two nodes compare.
    pub fn order_hash(&self) -> [u8; 32] {
        let mut acc = self.frozen_acc;
        for h in self.suffix_order() {
            acc = chain_hash(&acc, &h);
        }
        acc
    }

    /// True iff the block is in the active window or in the retained emitted
    /// set (blocks pruned more than `max_window` heights below final are
    /// forgotten).
    pub fn contains(&self, h: &BlockHash) -> bool {
        self.recs.contains_key(h) || self.emitted_at.contains_key(h)
    }

    /// Deduplicated, sorted worklist of parent hashes the parked views are
    /// waiting on — the caller's backfill queue.
    ///
    /// **Only ACTIONABLE hashes are returned.** A hash that is itself already
    /// parked in `pending` is excluded: the braid already holds that block, so
    /// re-inserting it returns [`InsertOutcome::Duplicate`] and the caller
    /// makes no progress. Reporting those was worse than useless — it buried
    /// the genuinely-unknown hashes among thousands of no-ops (measured: 1973
    /// requested, 1972 of them already parked, so a backfill loop driven by
    /// this list could never converge). Everything returned here is a block
    /// the braid does not hold in any form, and supplying it will unpark work.
    pub fn missing_parents(&self) -> Vec<BlockHash> {
        let mut out: Vec<BlockHash> = self
            .waiters
            .iter()
            .filter(|(p, kids)| {
                self.known_height(p).is_none()
                    && !self.pending.contains_key(*p)
                    && kids.iter().any(|k| self.pending.contains_key(k))
            })
            .map(|(p, _)| *p)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Window / pending / finalized / tips occupancy + rejection counters.
    pub fn stats(&self) -> BraidStats {
        let tips = self
            .recs
            .values()
            .filter(|r| {
                self.children
                    .get(&r.view.hash)
                    .is_none_or(|kids| kids.is_empty())
            })
            .count();
        BraidStats {
            window: self.recs.len(),
            ready: self.frontier.len(),
            pending: self.pending.len(),
            tips,
            emitted_total: self.frozen.len(),
            finalized_height: self.finalized_height(),
            below_final: self.below_final_count,
            rejected: self.rejected_count,
            dropped: self.dropped_count,
            dag_memory_bytes: self.dag.stats().memory_bytes,
        }
    }

    /// The view of a resident window block (present.rs uses this to read
    /// producers + merge edges along the linearization).
    pub(crate) fn view_of(&self, h: &BlockHash) -> Option<&BlockView> {
        self.recs.get(h).map(|r| &r.view)
    }
}

/// Occupancy + counter snapshot for a [`Braid`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BraidStats {
    /// Active (resident) window views, emitted-but-retained included.
    pub window: usize,
    /// Ready (all parents emitted) views awaiting finalization.
    pub ready: usize,
    /// Parked parent-missing views.
    pub pending: usize,
    /// Window blocks with no known children.
    pub tips: usize,
    /// Total blocks emitted into the frozen order.
    pub emitted_total: usize,
    /// Current finalized height (0 until the tip clears `final_depth`).
    pub finalized_height: u64,
    /// Inserts refused below the finality line (including pruned pendings).
    pub below_final: u64,
    /// Structurally rejected inserts.
    pub rejected: u64,
    /// Parked views dropped at unpark time (stale / invalid / window-full).
    pub dropped: u64,
    /// Bytes held by the bitfield substrate.
    pub dag_memory_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Offset by 1 so h(0) never collides with the all-zero genesis parent
    // (which would trip the self-parent reject).
    fn h(n: u8) -> BlockHash {
        [n + 1; 32]
    }

    const PA: [u8; 32] = [0xAA; 32];
    const PB: [u8; 32] = [0xBB; 32];

    fn v(
        hash: BlockHash,
        parent: BlockHash,
        merge_parents: Vec<BlockHash>,
        height: u64,
        producer: [u8; 32],
    ) -> BlockView {
        BlockView {
            hash,
            parent,
            merge_parents,
            height,
            producer,
        }
    }

    fn cfg(final_depth: u64) -> BraidConfig {
        BraidConfig {
            final_depth,
            ..BraidConfig::default()
        }
    }

    fn genesis() -> BlockView {
        v(h(0), [0u8; 32], vec![], 0, PA)
    }

    /// Linear chain h(0)..h(n) under producer PA.
    fn chain_views(n: u8) -> Vec<BlockView> {
        let mut out = vec![genesis()];
        for i in 1..=n {
            out.push(v(h(i), h(i - 1), vec![], i as u64, PA));
        }
        out
    }

    #[test]
    fn spine_selection_max_height_min_hash() {
        let mut b = Braid::new(cfg(64));
        for view in chain_views(3) {
            assert!(matches!(b.insert(view), InsertOutcome::Inserted { .. }));
        }
        // Shorter fork off h(1) by PB.
        assert!(matches!(
            b.insert(v(h(10), h(1), vec![], 2, PB)),
            InsertOutcome::Inserted { .. }
        ));
        // Max height wins.
        assert_eq!(b.selected_tip(), Some(h(3)));
        assert!(b.is_on_spine(&h(0)));
        assert!(b.is_on_spine(&h(2)));
        assert!(b.is_on_spine(&h(3)));
        assert!(!b.is_on_spine(&h(10))); // off-spine fork

        // Same-height competitors: the SMALLER hash takes the tip.
        let mut b2 = Braid::new(cfg(64));
        for view in chain_views(2) {
            b2.insert(view);
        }
        b2.insert(v(h(9), h(2), vec![], 3, PA));
        b2.insert(v(h(5), h(2), vec![], 3, PB));
        // Two height-3 candidates: h(5) < h(9) byte-wise → h(5) selected.
        assert_eq!(b2.selected_tip(), Some(h(5)));
        assert!(b2.is_on_spine(&h(5)));
        assert!(!b2.is_on_spine(&h(9)));
    }

    #[test]
    fn merge_tips_excludes_own_tip_and_orders_deterministically() {
        let mut b = Braid::new(cfg(64));
        b.insert(genesis());
        b.insert(v(h(1), h(0), vec![], 1, PA));
        b.insert(v(h(2), h(0), vec![], 1, PB));
        // Tips: h(1), h(2).
        assert_eq!(b.merge_tips(&h(1), 4), vec![h(2)]);
        assert_eq!(b.merge_tips(&h(2), 4), vec![h(1)]);
        // Extend PA's branch: tips = h(3)@2, h(2)@1 → height desc.
        b.insert(v(h(3), h(1), vec![], 2, PA));
        assert_eq!(b.merge_tips(&h(200), 4), vec![h(3), h(2)]);
        assert_eq!(b.merge_tips(&h(3), 4), vec![h(2)]);
        // Cap respected.
        assert_eq!(b.merge_tips(&h(200), 1), vec![h(3)]);
    }

    #[test]
    fn base_anchored_seed_chains_and_finalizes() {
        // Pruned-window scenario: base at height 1000, its ancestry unknown.
        let base = h(9);
        let mut b = Braid::new_with_base(cfg(64), base, 1000);
        // A 200-block suffix chaining from the base must insert cleanly...
        let mut parent = base;
        for i in 0..200u64 {
            let hash = {
                let mut x = [0u8; 32];
                x[..8].copy_from_slice(&(i + 7).to_le_bytes());
                x[31] = 0x5A;
                x
            };
            match b.insert(v(hash, parent, vec![], 1001 + i, PA)) {
                InsertOutcome::Inserted { .. } => {}
                other => panic!("seed insert at H={} got {other:?}", 1001 + i),
            }
            parent = hash;
        }
        // ...finality advances past the base (tip 1200 − depth 64 = 1136)...
        assert_eq!(b.finalized_height(), 1136);
        // ...and the anchor itself is never emitted.
        let drained = b.drain_ordered();
        assert!(!drained.is_empty());
        assert!(!drained.contains(&base));
        // A block below the base with unknown ancestry parks, it does not wedge.
        match b.insert(v(h(8), h(7), vec![], 900, PB)) {
            InsertOutcome::MissingParents(_) | InsertOutcome::BelowFinal { .. } => {}
            other => panic!("below-base insert got {other:?}"),
        }
    }

    #[test]
    fn below_final_guard_refuses_and_leaves_order_untouched() {
        let mut b = Braid::new(cfg(4));
        for view in chain_views(10) {
            assert!(matches!(b.insert(view), InsertOutcome::Inserted { .. }));
        }
        // tip=10, final_depth=4 → finalized height 6.
        assert_eq!(b.finalized_height(), 6);
        let drained = b.drain_ordered();
        assert_eq!(drained.len(), 7); // heights 0..=6
        assert_eq!(drained, chain_views(6).iter().map(|v| v.hash).collect::<Vec<_>>());

        let before_lin = b.linearize();
        let before_oh = b.order_hash();
        // Fork at height 3 (≤ finalized 6) → refused.
        match b.insert(v(h(99), h(2), vec![], 3, PB)) {
            InsertOutcome::BelowFinal { finalized } => assert_eq!(finalized, 6),
            other => panic!("expected BelowFinal, got {other:?}"),
        }
        assert_eq!(b.linearize(), before_lin);
        assert_eq!(b.order_hash(), before_oh);
        assert!(b.drain_ordered().is_empty());
    }

    /// Two-producer braid with cross merges — the live dag_mode shape.
    fn braid_specs() -> Vec<BlockView> {
        vec![
            genesis(),
            v(h(1), h(0), vec![], 1, PA),
            v(h(2), h(0), vec![], 1, PB),
            v(h(3), h(1), vec![h(2)], 2, PA), // merge block
            v(h(4), h(2), vec![], 2, PB),
            v(h(5), h(3), vec![h(4)], 3, PA),
            v(h(6), h(4), vec![h(3)], 3, PB),
            v(h(7), h(5), vec![h(6)], 4, PA),
        ]
    }

    #[test]
    fn incremental_equals_batch_across_arrival_orders() {
        let specs = braid_specs();
        // Order 1: as-built. Order 2: PB's blocks first (valid parked/unpark
        // exercise — children before parents).
        let order1: Vec<usize> = (0..specs.len()).collect();
        let order2: Vec<usize> = vec![7, 6, 5, 4, 3, 2, 1, 0];

        let mut results = Vec::new();
        for order in [order1, order2] {
            let mut b = Braid::new(cfg(2));
            let mut drains: Vec<BlockHash> = Vec::new();
            for i in order {
                b.insert(specs[i].clone());
                drains.extend(b.drain_ordered());
            }
            // Drains are a prefix of the batch linearization.
            let lin = b.linearize();
            assert_eq!(drains.as_slice(), &lin[..drains.len()]);
            // Extend the spine so every original block finalizes.
            let mut parent = h(7);
            for i in 0..6u8 {
                let hh = h(100 + i);
                assert!(matches!(
                    b.insert(v(hh, parent, vec![], 5 + i as u64, PA)),
                    InsertOutcome::Inserted { .. }
                ));
                parent = hh;
            }
            drains.extend(b.drain_ordered());
            let lin = b.linearize();
            assert_eq!(drains.as_slice(), &lin[..drains.len()]);
            // All 8 original blocks are in the drained (finalized) prefix.
            for spec in &specs {
                assert!(drains.contains(&spec.hash), "missing {:?}", spec.hash[0]);
            }
            results.push((lin, b.order_hash()));
        }
        // Both arrival orders converge on identical order + order_hash.
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn structural_rejects_and_duplicate() {
        let mut b = Braid::new(cfg(64));
        b.insert(genesis());
        assert_eq!(b.insert(genesis()), InsertOutcome::Duplicate);
        // Self-parent.
        assert_eq!(
            b.insert(v(h(1), h(1), vec![], 1, PA)),
            InsertOutcome::Rejected("self-parent")
        );
        // Too many merge parents (max 4).
        assert_eq!(
            b.insert(v(
                h(1),
                h(0),
                vec![h(11), h(12), h(13), h(14), h(15)],
                1,
                PA
            )),
            InsertOutcome::Rejected("too many merge parents")
        );
        // Duplicate merge parent.
        assert_eq!(
            b.insert(v(h(1), h(0), vec![h(11), h(11)], 1, PA)),
            InsertOutcome::Rejected("duplicate merge parent")
        );
        // Merge parent duplicating the spine parent.
        assert_eq!(
            b.insert(v(h(1), h(0), vec![h(0)], 1, PA)),
            InsertOutcome::Rejected("merge parent duplicates spine parent")
        );
        // Bad height (parent is height 0, child claims 5).
        assert_eq!(
            b.insert(v(h(1), h(0), vec![], 5, PA)),
            InsertOutcome::Rejected("height not parent height + 1")
        );
        assert_eq!(b.stats().rejected, 5);
    }

    #[test]
    fn missing_parents_park_and_unpark_cascade() {
        let mut b = Braid::new(cfg(64));
        b.insert(genesis());
        // Grandchild first: parent h(1) unknown.
        assert_eq!(
            b.insert(v(h(2), h(1), vec![], 2, PA)),
            InsertOutcome::MissingParents(vec![h(1)])
        );
        assert_eq!(b.missing_parents(), vec![h(1)]);
        assert!(!b.contains(&h(2)));
        // Parent arrives: itself + the parked child enter the window.
        assert_eq!(
            b.insert(v(h(1), h(0), vec![], 1, PA)),
            InsertOutcome::Inserted { newly_ready: 2 }
        );
        assert!(b.missing_parents().is_empty());
        assert!(b.contains(&h(2)));
        assert_eq!(b.linearize(), vec![h(0), h(1), h(2)]);
        assert_eq!(b.stats().pending, 0);
    }

    /// REGRESSION: `missing_parents()` must never name a block the braid is
    /// already holding in its own pending set. Re-inserting such a hash
    /// returns `Duplicate`, so a caller driving a backfill loop off this
    /// worklist makes no progress and spins forever. Measured in the wild at
    /// P=6/k=1: 1973 hashes requested, 1972 of them already parked.
    #[test]
    fn missing_parents_reports_only_actionable_hashes() {
        let mut b = Braid::new(cfg(64));
        b.insert(genesis());

        // Deliver a 3-deep chain in reverse: h(3) parks on h(2), h(2) parks
        // on h(1). h(1) is the ONLY block the braid does not hold.
        assert_eq!(
            b.insert(v(h(3), h(2), vec![], 3, PA)),
            InsertOutcome::MissingParents(vec![h(2)])
        );
        assert_eq!(
            b.insert(v(h(2), h(1), vec![], 2, PA)),
            InsertOutcome::MissingParents(vec![h(1)])
        );
        assert_eq!(b.stats().pending, 2);

        // h(2) is parked, so naming it would be unactionable noise; only the
        // genuinely-absent h(1) may be reported.
        let work = b.missing_parents();
        assert_eq!(work, vec![h(1)], "worklist must exclude already-parked h(2)");

        // Every hash returned must actually make progress when supplied —
        // that is what "actionable" means.
        for hash in &work {
            assert!(
                !matches!(b.insert(v(*hash, h(0), vec![], 1, PA)), InsertOutcome::Duplicate),
                "worklist named a hash the braid already holds"
            );
        }

        // And supplying it drains the whole parked chain.
        assert!(b.missing_parents().is_empty());
        assert_eq!(b.stats().pending, 0);
        assert_eq!(b.linearize(), vec![h(0), h(1), h(2), h(3)]);
    }

    /// The finality line must not freeze a height whose block is still parked
    /// — `cleanup` would destroy that view permanently and every descendant
    /// with it. Bounded by `max_window` so a single bogus low view cannot
    /// stall finality forever.
    #[test]
    fn finality_does_not_outrun_the_parked_set() {
        let mut b = Braid::new(cfg(4));
        b.insert(genesis());
        // A parked view at height 2 (its parent h(1) never arrives).
        assert!(matches!(
            b.insert(v(h(2), h(1), vec![], 2, PA)),
            InsertOutcome::MissingParents(_)
        ));
        // Build a resident chain far above it off genesis.
        let mut prev = h(0);
        for n in 10u8..30 {
            let view = v(h(n), prev, vec![], (n - 9) as u64, PB);
            b.insert(view);
            prev = h(n);
        }
        // Tip is well past final_depth=4, but height 2 is still parked, so the
        // line must stay strictly below it rather than freezing over it.
        assert!(
            b.finalized_height() < 2,
            "finality {} froze a height with a parked block",
            b.finalized_height()
        );
        assert_eq!(b.stats().pending, 1, "the parked view must survive");
    }

    #[test]
    fn order_hash_tracks_linearization() {
        let mut a = Braid::new(cfg(64));
        let mut b = Braid::new(cfg(64));
        for view in braid_specs() {
            a.insert(view);
        }
        for view in braid_specs().into_iter().rev() {
            b.insert(view);
        }
        assert_eq!(a.linearize(), b.linearize());
        assert_eq!(a.order_hash(), b.order_hash());
        // A diverging insert changes the order hash.
        let before = a.order_hash();
        a.insert(v(h(50), h(7), vec![], 5, PB));
        assert_ne!(a.order_hash(), before);
    }
}
