//! Integration proof: the codec=4 trailer `archive_root` produced by the serve
//! side is byte-identical to what the sigil-top client `SnapshotVerifier`
//! computes — using the SAME `bincode` encoder that produces the on-wire bytes,
//! not just the field-by-field fold. If this drifts, snapshot finalize breaks on
//! the live network.

use sigil_header::SkeletonRecord;
use sigil_serve::{archive_root_range, build_trailer, ArchiveRootCache, BlockSkeletonSource};

struct MemChain {
    recs: Vec<SkeletonRecord>,
}
impl MemChain {
    fn new(n: u64) -> Self {
        let mut recs = Vec::new();
        let mut parent = [0u8; 32];
        for h in 0..n {
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

/// Exactly the client's recomputation path, but via `bincode::serialize` — the
/// real on-wire encoder. This is what a from-scratch verifier would do.
fn client_root_via_bincode(chain: &MemChain, from: u64, to: u64) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for height in from..=to {
        let bytes = bincode::serialize(&chain.recs[height as usize]).unwrap();
        assert_eq!(bytes.len(), 72, "wire record must be 72 B");
        h.update(&bytes);
    }
    *h.finalize().as_bytes()
}

#[test]
fn trailer_root_equals_bincode_client_root() {
    let chain = MemChain::new(5_000);
    let t = build_trailer(&chain, 0, 4_999);
    assert_eq!(t.archive_root, client_root_via_bincode(&chain, 0, 4_999));
}

#[test]
fn cache_root_equals_bincode_client_root_across_checkpoints() {
    let chain = MemChain::new(160_000);
    let mut cache = ArchiveRootCache::with_interval(50_000);
    for &anchor in &[12_345u64, 49_999, 50_000, 99_999, 130_000, 159_999] {
        assert_eq!(
            cache.trailer_for(&chain, 0, anchor).archive_root,
            client_root_via_bincode(&chain, 0, anchor),
            "cache root mismatch at anchor {anchor}"
        );
    }
}

#[test]
fn anchor_pinned_not_tip_pinned_integration() {
    // Client fixed anchor=39_999 from the 'P' header, then the producer's tip
    // grew well past the 32768 serve window during paging.
    let anchor = 39_999u64;
    let at_serve = build_trailer(&MemChain::new(40_000), 0, anchor).archive_root;
    let after_growth = build_trailer(&MemChain::new(250_000), 0, anchor).archive_root;
    assert_eq!(at_serve, after_growth);
    assert_eq!(
        after_growth,
        archive_root_range(&MemChain::new(250_000), 0, anchor)
    );
}
