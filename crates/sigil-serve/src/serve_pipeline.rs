//! Producer/serve side of the SIGIL codec=2/3/4 snapshot wire.
//!
//! Background — the **codec=4 trailer activation blocker**: the inline responder
//! once computed the trailer's `archive_root` over `[0..=chain.height()-1]` (the
//! *current* tip at serve time), while the client's `SnapshotVerifier` (sigil-top
//! `block_sync::fetch`) folds in exactly the records `[base..=anchor_height]` it
//! learned from the phase-(a) `'P'` header. On any sync long enough that the tip
//! advances during paging — every chain larger than the ~32768-header serve
//! window — the server root covered MORE records than the client hashed, so
//! `finalize()` returned `RootMismatch`, the pull aborted, and the client
//! silently downgraded to the slow codec=1 crawl. The one-line correctness fix
//! (hash over the full `[0..anchor]` prefix) **landed in HEAD as `239d158`**.
//!
//! This module is the **additive serve-RATE** layer on top of that fix:
//! [`build_trailer`] / [`archive_root_range`] hash **exactly the requested
//! `[from..=to]`** (honoring the client's anchor, never the live tip),
//! byte-identical to `SnapshotVerifier::push`; and [`ArchiveRootCache`] memoizes
//! the immutable finalized prefix so a `[0..=anchor]` finalize is O(tail), not
//! O(chain), per request — turning the landed correct-but-O(chain) trailer into a
//! ≥100k-serve-rate one. sigil-node adopts `ArchiveRootCache` via the seam
//! coordinated with rocky-sync-B (#649).
//!
//! This module is transport-free and node-free: it speaks only
//! [`BlockSkeletonSource`], so it unit-tests without a live node and the
//! sigil-node seam is a ~10-line adapter (see `BlockSkeletonSource` docs).

use sigil_header::{
    BlockHash, SkeletonRecord, SnapshotHeader, SnapshotTrailer, SNAPSHOT_MAGIC, SNAPSHOT_VERSION,
};

/// Default checkpoint interval for [`ArchiveRootCache`] (records between cloned
/// hasher snapshots). 50k mirrors the client's `PAGE` so a thawed tail is at
/// most one page of re-hash (~17ms on a modern core per DeepSeek). Tunable via
/// [`ArchiveRootCache::with_interval`].
pub const DEFAULT_CKPT_INTERVAL: u64 = 50_000;

/// A height-addressable source of skeleton records — the producer's chain.
///
/// sigil-node implements this with a ~10-line adapter over its `(chain,
/// chain_log)` pair, mirroring the existing gather at main.rs L892-899/937:
///
/// ```ignore
/// struct ServeSrc<'a> { chain: &'a Chain, chain_log: &'a ChainLog }
/// impl sigil_serve::BlockSkeletonSource for ServeSrc<'_> {
///     fn skeleton_at(&self, h: u64) -> Option<SkeletonRecord> {
///         // window first, then disk — same order the responder already uses
///         if let Some(b) = self.chain.get(h) {
///             return Some(SkeletonRecord::from_header(&b.header));
///         }
///         self.chain_log.get_range(h, h).first()
///             .map(|b| SkeletonRecord::from_header(&b.header))
///     }
///     fn tip(&self) -> u64 { self.chain.height().saturating_sub(1) }
/// }
/// ```
pub trait BlockSkeletonSource {
    /// The skeleton record committed at `height`, or `None` if this node does
    /// not have it (a gap, or above tip). MUST be the same `SkeletonRecord` the
    /// client verifies: `{ height, block_hash = full-header BLAKE3, parent_hash }`.
    fn skeleton_at(&self, height: u64) -> Option<SkeletonRecord>;

    /// The producer's current tip height (last applied). Used only to clamp a
    /// caller's `to`; never used to *choose* the root range (that's the bug).
    fn tip(&self) -> u64;

    /// Records for `[from..=to]` in ascending height order, stopping at the first
    /// gap. Default loops [`skeleton_at`]; override for a single sequential read.
    fn skeleton_range(&self, from: u64, to: u64) -> Vec<SkeletonRecord> {
        let mut out = Vec::new();
        if to < from {
            return out;
        }
        let mut h = from;
        loop {
            match self.skeleton_at(h) {
                Some(r) => out.push(r),
                None => break,
            }
            if h == to {
                break;
            }
            h += 1;
        }
        out
    }
}

/// Fold one record into a BLAKE3 hasher in the **canonical wire layout** —
/// `height_le ‖ block_hash ‖ parent_hash` (72 B). This is byte-identical to
/// `bincode::serialize(rec)` (default LE fixint, fixed arrays unprefixed) AND to
/// the client's `SnapshotVerifier::push`, so the producer root matches the
/// verifier root exactly. Changing this breaks the wire contract — don't.
#[inline]
pub fn fold_record(hasher: &mut blake3::Hasher, rec: &SkeletonRecord) {
    hasher.update(&rec.height.to_le_bytes());
    hasher.update(&rec.block_hash);
    hasher.update(&rec.parent_hash);
}

/// Compute the archive root over **exactly** `[from..=to]` (stateless). This is
/// the correctness primitive: the root depends only on the requested range, not
/// on the producer's current tip. Stops at the first gap (root then covers only
/// the contiguous prefix actually present — the client's `CountMismatch` /
/// `RootMismatch` will reject a short serve, which is the safe outcome).
pub fn archive_root_range<S: BlockSkeletonSource + ?Sized>(src: &S, from: u64, to: u64) -> BlockHash {
    let mut h = blake3::Hasher::new();
    if to >= from {
        // Scan in bounded chunks via skeleton_range (one sequential read per chunk
        // on a disk source) instead of per-height — kills cold-start I/O thrash —
        // while capping resident records at ~SCAN_CHUNK (≈3.6 MB) so a genesis-wide
        // [0..=11M] root never materializes the whole chain.
        const SCAN_CHUNK: u64 = 50_000;
        let mut next = from;
        loop {
            let chunk_to = next.saturating_add(SCAN_CHUNK - 1).min(to);
            let want = chunk_to - next + 1;
            let recs = src.skeleton_range(next, chunk_to);
            if recs.is_empty() {
                break;
            }
            for rec in &recs {
                fold_record(&mut h, rec);
            }
            if (recs.len() as u64) < want || chunk_to == to {
                break; // gap inside the chunk, or reached the end
            }
            next = chunk_to + 1;
        }
    }
    *h.finalize().as_bytes()
}

/// Build the codec=4 `'F'` trailer for the client's requested `[from..=to]`.
///
/// THE FIX: `to` is the client's requested anchor (from the `'P'` header it
/// already paged against), NOT `src.tip()`. The caller (sigil-node seam) should
/// pass `req.to.min(src.tip())` so a client can't request beyond our tip, but
/// MUST NOT apply the headers serve-cap here — the trailer must span the whole
/// `[from..=to]` or the client's count/root check fails.
///
/// M1: `anchor_sig` / `fold_blob` are empty (structural pull the client accepts
/// on root-match). M2 (LANE-B/DNS-anchor) fills them — additive, no wire change.
pub fn build_trailer<S: BlockSkeletonSource + ?Sized>(src: &S, from: u64, to: u64) -> SnapshotTrailer {
    SnapshotTrailer {
        archive_root: archive_root_range(src, from, to),
        anchor_sig: Vec::new(),
        fold_blob: Vec::new(),
    }
}

/// Build the codec=3 `'P'` discovery header advertising `[0..=tip]`. Centralizes
/// what the responder does inline at main.rs L908-915 so the framing lives in
/// one place. TIP-dependent — never cache it.
pub fn snapshot_header<S: BlockSkeletonSource + ?Sized>(src: &S) -> SnapshotHeader {
    let top = src.tip();
    let anchor_hash = src.skeleton_at(top).map(|r| r.block_hash).unwrap_or([0u8; 32]);
    SnapshotHeader {
        magic: SNAPSHOT_MAGIC,
        version: SNAPSHOT_VERSION,
        base_height: 0,
        anchor_height: top,
        anchor_hash,
        count: top.saturating_add(1),
    }
}

/// Serialize a codec=2 `'S'` skeleton page for `[from..=to]`: `b'S'` ‖
/// `bincode(Vec<SkeletonRecord>)`. Matches the responder at main.rs L924-927.
pub fn skeleton_page<S: BlockSkeletonSource + ?Sized>(src: &S, from: u64, to: u64) -> Vec<u8> {
    let recs = src.skeleton_range(from, to);
    let mut o = vec![b'S'];
    o.extend(bincode::serialize(&recs).unwrap_or_default());
    o
}

/// Incremental memoizer for the immutable finalized prefix root `[0..=to]`.
///
/// Holds cloned `blake3::Hasher` snapshots at every `interval` heights (the only
/// sound way to memoize BLAKE3 — its public API is not resume-from-bytes, but
/// `Hasher: Clone` is O(1) and exact). A `root_prefix(to)` query clones the
/// nearest checkpoint `≤ to` and folds the tail `[ckpt+1..=to]`, so cost is
/// O(tail) ≤ `interval`, not O(chain). One-time warm-up feeds `[0..=to]` once.
///
/// Single-threaded by design: the sigil-node responder loop is a single
/// `select!`, so the cache is held by value across iterations — no lock. Snapshots
/// are RAM-only (clone isn't serializable) and ~16 KB each (≈ a few MB for an
/// 11M chain at 50k spacing).
///
/// Soundness note: only valid for the **finalized** prefix. The caller must only
/// memoize ranges fully below the live window (`to < window_base`); records there
/// never mutate. Serving the live tip stays on [`build_trailer`] (uncached).
pub struct ArchiveRootCache {
    interval: u64,
    /// (height, hasher-state-after-folding-[0..=height]) ascending by height.
    checkpoints: Vec<(u64, blake3::Hasher)>,
    /// Rolling hasher folded contiguously over `[0..=fed_upto]`.
    rolling: blake3::Hasher,
    /// Highest height folded into `rolling`, or `None` if empty.
    fed_upto: Option<u64>,
}

impl Default for ArchiveRootCache {
    fn default() -> Self {
        Self::with_interval(DEFAULT_CKPT_INTERVAL)
    }
}

impl ArchiveRootCache {
    /// New cache with the default checkpoint interval.
    pub fn new() -> Self {
        Self::default()
    }

    /// New cache with a custom checkpoint interval (records between snapshots).
    /// `interval` of 0 is treated as 1.
    pub fn with_interval(interval: u64) -> Self {
        Self {
            interval: interval.max(1),
            checkpoints: Vec::new(),
            rolling: blake3::Hasher::new(),
            fed_upto: None,
        }
    }

    /// Number of checkpoint snapshots currently held (for diagnostics/tests).
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    /// Extend the rolling hasher (and its checkpoints) forward to cover `[0..=to]`,
    /// reading any not-yet-folded records from `src`. Stops at the first gap.
    ///
    /// Reads in `interval`-sized batches via `skeleton_range` (one sequential read
    /// per chunk on a disk source — no per-height I/O thrash on the cold warm-up),
    /// folding each record and snapshotting a cloned hasher at every checkpoint
    /// boundary. Memory stays bounded at one chunk (~3.6 MB at 50k×72 B).
    fn extend_to<S: BlockSkeletonSource + ?Sized>(&mut self, src: &S, to: u64) {
        let mut next = match self.fed_upto {
            Some(h) if h >= to => return,
            Some(h) => h.saturating_add(1),
            None => 0,
        };
        let chunk = self.interval.max(1);
        while next <= to {
            let chunk_to = next.saturating_add(chunk - 1).min(to);
            let want = chunk_to - next + 1;
            let recs = src.skeleton_range(next, chunk_to);
            if recs.is_empty() {
                break; // gap at `next`: don't fabricate; queries past it fall back
            }
            for rec in &recs {
                fold_record(&mut self.rolling, rec);
                self.fed_upto = Some(next);
                // Snapshot AT the checkpoint boundary, immediately after folding it,
                // before the next record — so the clone is exactly [0..=next].
                if (next + 1) % self.interval == 0 {
                    self.checkpoints.push((next, self.rolling.clone()));
                }
                if next == u64::MAX {
                    return;
                }
                next += 1;
            }
            if (recs.len() as u64) < want {
                break; // short read ⇒ gap inside the chunk
            }
        }
    }

    /// The nearest checkpoint at height `≤ to`: returns `(start_height, hasher)`
    /// where `hasher` already covers `[0..=start_height]`, so the caller folds
    /// `[start_height+1..=to]`. `None` → fold from genesis.
    fn nearest_checkpoint(&self, to: u64) -> Option<(u64, blake3::Hasher)> {
        // checkpoints is ascending; take the greatest height <= to.
        let idx = self.checkpoints.partition_point(|(h, _)| *h <= to);
        if idx == 0 {
            None
        } else {
            let (h, ref hasher) = self.checkpoints[idx - 1];
            Some((h, hasher.clone()))
        }
    }

    /// Archive root over the finalized prefix `[0..=to]`, using cached checkpoints.
    /// Equivalent to `archive_root_range(src, 0, to)` but O(tail), not O(chain).
    pub fn root_prefix<S: BlockSkeletonSource + ?Sized>(&mut self, src: &S, to: u64) -> BlockHash {
        self.extend_to(src, to);
        let (mut hasher, start) = match self.nearest_checkpoint(to) {
            Some((ckpt_h, hasher)) => (hasher, ckpt_h + 1),
            None => (blake3::Hasher::new(), 0),
        };
        // Tail is bounded by `interval` (checkpoints are that dense up to fed_upto,
        // and skeleton_range stops at any gap), so this is one bounded batched read.
        if to >= start {
            for rec in src.skeleton_range(start, to) {
                fold_record(&mut hasher, &rec);
            }
        }
        *hasher.finalize().as_bytes()
    }

    /// Build the codec=4 trailer using the cache when `from == 0` (the only shape
    /// the client ever requests — `'P'.base_height` is always 0), else fall back
    /// to the stateless [`build_trailer`].
    pub fn trailer_for<S: BlockSkeletonSource + ?Sized>(
        &mut self,
        src: &S,
        from: u64,
        to: u64,
    ) -> SnapshotTrailer {
        if from == 0 {
            SnapshotTrailer {
                archive_root: self.root_prefix(src, to),
                anchor_sig: Vec::new(),
                fold_blob: Vec::new(),
            }
        } else {
            build_trailer(src, from, to)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory chain of synthetic, correctly-linked skeleton records.
    struct MemChain {
        recs: Vec<SkeletonRecord>,
    }
    impl MemChain {
        fn new(n: u64) -> Self {
            let mut recs = Vec::new();
            let mut parent = [0u8; 32];
            for h in 0..n {
                // deterministic, unique block_hash per height
                let block_hash = *blake3::hash(&h.to_le_bytes()).as_bytes();
                recs.push(SkeletonRecord { height: h, block_hash, parent_hash: parent });
                parent = block_hash;
            }
            Self { recs }
        }
    }
    impl BlockSkeletonSource for MemChain {
        fn skeleton_at(&self, height: u64) -> Option<SkeletonRecord> {
            self.recs.get(height as usize).cloned()
        }
        fn tip(&self) -> u64 {
            self.recs.len().saturating_sub(1) as u64
        }
    }

    /// The trailer root MUST equal the client `SnapshotVerifier`'s independent
    /// recomputation (height_le ‖ block_hash ‖ parent_hash, folded in order).
    fn client_root(chain: &MemChain, from: u64, to: u64) -> BlockHash {
        let mut h = blake3::Hasher::new();
        for height in from..=to {
            let r = &chain.recs[height as usize];
            h.update(&r.height.to_le_bytes());
            h.update(&r.block_hash);
            h.update(&r.parent_hash);
        }
        *h.finalize().as_bytes()
    }

    #[test]
    fn trailer_matches_client_root_for_exact_range() {
        let chain = MemChain::new(1000);
        let t = build_trailer(&chain, 0, 999);
        assert_eq!(t.archive_root, client_root(&chain, 0, 999));
        assert!(t.anchor_sig.is_empty() && t.fold_blob.is_empty());
    }

    /// THE BUG REGRESSION: tip advances after the client fixed its anchor. The
    /// trailer over [0..=anchor] must NOT shift when the tip grows.
    #[test]
    fn trailer_root_is_anchor_pinned_not_tip_pinned() {
        let short = MemChain::new(40_000); // client paged [0..=39_999]
        let anchor = 39_999u64;
        let root_at_serve = build_trailer(&short, 0, anchor).archive_root;

        // tip advances past the 32768 serve window while the client was paging.
        let grown = MemChain::new(120_000);
        let root_after_growth = build_trailer(&grown, 0, anchor).archive_root;

        assert_eq!(
            root_at_serve, root_after_growth,
            "trailer must be pinned to the requested anchor, not the live tip"
        );
        // And it equals what the client computed over exactly its pushed records.
        assert_eq!(root_after_growth, client_root(&grown, 0, anchor));
    }

    /// bincode(SkeletonRecord) must equal the field-by-field fold (72 B, no prefix).
    #[test]
    fn fold_is_byte_identical_to_bincode() {
        let chain = MemChain::new(8);
        let mut a = blake3::Hasher::new();
        let mut b = blake3::Hasher::new();
        for r in &chain.recs {
            fold_record(&mut a, r);
            let bytes = bincode::serialize(r).unwrap();
            assert_eq!(bytes.len(), 72, "SkeletonRecord must be 72 B on the wire");
            b.update(&bytes);
        }
        assert_eq!(a.finalize().as_bytes(), b.finalize().as_bytes());
    }

    /// The cache must produce identical roots to the stateless path, across
    /// checkpoint boundaries and for both warm-forward and backward queries.
    #[test]
    fn cache_root_equals_stateless_root() {
        let chain = MemChain::new(130_000);
        let mut cache = ArchiveRootCache::with_interval(50_000);

        // warm forward to a high anchor (creates checkpoints at 49_999, 99_999)
        let hi = 129_999u64;
        assert_eq!(cache.root_prefix(&chain, hi), archive_root_range(&chain, 0, hi));
        assert!(cache.checkpoint_count() >= 2);

        // a backward query (to < fed_upto) must thaw the nearest checkpoint
        let mid = 70_000u64;
        assert_eq!(cache.root_prefix(&chain, mid), archive_root_range(&chain, 0, mid));

        // exactly on a checkpoint boundary
        let on_ckpt = 99_999u64;
        assert_eq!(cache.root_prefix(&chain, on_ckpt), archive_root_range(&chain, 0, on_ckpt));

        // below the first checkpoint (no snapshot available → genesis fold)
        let low = 10_000u64;
        assert_eq!(cache.root_prefix(&chain, low), archive_root_range(&chain, 0, low));
    }

    /// A source that does NOT override `skeleton_range` (falls back to the trait's
    /// per-height default). The batched `extend_to` / `archive_root_range` must
    /// still produce identical roots through that default path.
    #[test]
    fn batched_paths_agree_via_default_skeleton_range() {
        struct NoBatch(MemChain);
        impl BlockSkeletonSource for NoBatch {
            fn skeleton_at(&self, h: u64) -> Option<SkeletonRecord> {
                self.0.skeleton_at(h)
            }
            fn tip(&self) -> u64 {
                self.0.tip()
            }
            // intentionally NO skeleton_range override → uses trait default
        }
        let src = NoBatch(MemChain::new(120_001));
        let oracle = MemChain::new(120_001);
        let mut cache = ArchiveRootCache::with_interval(50_000);
        for &anchor in &[0u64, 1, 49_999, 50_000, 99_999, 120_000] {
            assert_eq!(
                cache.root_prefix(&src, anchor),
                client_root(&oracle, 0, anchor),
                "default-skeleton_range cache root mismatch at {anchor}"
            );
            assert_eq!(archive_root_range(&src, 0, anchor), client_root(&oracle, 0, anchor));
        }
    }

    /// trailer_for via the cache equals the stateless trailer for from==0.
    #[test]
    fn cached_trailer_equals_stateless_trailer() {
        let chain = MemChain::new(60_000);
        let mut cache = ArchiveRootCache::with_interval(50_000);
        let anchor = 59_999u64;
        assert_eq!(
            cache.trailer_for(&chain, 0, anchor).archive_root,
            build_trailer(&chain, 0, anchor).archive_root
        );
    }

    #[test]
    fn header_advertises_full_prefix() {
        let chain = MemChain::new(500);
        let h = snapshot_header(&chain);
        assert_eq!(h.base_height, 0);
        assert_eq!(h.anchor_height, 499);
        assert_eq!(h.count, 500);
        assert_eq!(h.anchor_hash, chain.recs[499].block_hash);
        assert_eq!(h.magic, SNAPSHOT_MAGIC);
    }
}
