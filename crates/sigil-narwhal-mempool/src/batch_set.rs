//! batch_set.rs — Phase F, "BatchSetRoot sidecars" (SIGIL_BRAIDPOOL_v1_1.md
//! §12, §17): the actual committed-TPS lever. A block committing to a
//! `BatchSetRoot` (one Merkle root over a handful of `BatchRefV1` digests)
//! can reference thousands of batches' worth of transactions without its own
//! header growing at all — decoupling block cadence from raw transaction
//! volume, which is the whole point of the Narwhal/BraidPool split.
//!
//! Deliberately NOT wired into `sigil-node`'s actual `BlockHeader` — that's a
//! real block-schema change to a live-mining chain, which needs its own
//! height-gate (`SIGIL_BRAIDPOOL_v1_1.md`'s own mainnet-safety discipline,
//! borrowed a level early) and is out of scope for this pass. See
//! [`body_mode`] for the activation gate that would guard it, and its own
//! doc comment for exactly why `n < 4` makes activation unsafe today.

use crate::merkle::merkle_root;
use crate::types::WorkerId;

/// One batch's presence in a block's committed set: its identity, the
/// availability evidence that backs it, and how many ops it carries (so a
/// verifier can bound total committed work without decoding every batch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BatchRefV1 {
    pub batch_id: [u8; 32],
    pub certificate_hash: [u8; 32],
    pub worker: WorkerId,
    pub tx_count: u32,
}

/// The full set a block commits to. What actually goes in the header is just
/// [`batch_set_root`]'s output; `refs` travels as a sidecar (§12: "the
/// sidecar carries the refs and certificates. The header stays essentially
/// constant-size").
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BatchSetV1 {
    pub refs: Vec<BatchRefV1>,
}

impl BatchSetV1 {
    pub fn total_tx_count(&self) -> u64 { self.refs.iter().map(|r| r.tx_count as u64).sum() }
    pub fn len(&self) -> usize { self.refs.len() }
    pub fn is_empty(&self) -> bool { self.refs.is_empty() }
}

/// Domain-separated leaf encoding for one `BatchRefV1` — hashed individually
/// before feeding [`merkle_root`], same pattern as `canonical::BatchHeaderV1`
/// (canonical struct encoding, not a hand-rolled field concatenation).
fn ref_leaf_hash(r: &BatchRefV1) -> [u8; 32] {
    let bytes = serde_json::to_vec(r).unwrap_or_default();
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/BATCHREF/V1");
    h.update(&bytes);
    h.finalize().into()
}

/// The ONE value a block header would actually carry for a committed batch
/// set (§12): a Merkle root over every ref's domain-separated leaf hash.
/// Empty set -> all-zero root (a block with zero batches committed, which is
/// a legitimate — if unusual — state, matching `merkle::merkle_root`'s own
/// convention for empty input).
pub fn batch_set_root(set: &BatchSetV1) -> [u8; 32] {
    let leaves: Vec<[u8; 32]> = set.refs.iter().map(ref_leaf_hash).collect();
    merkle_root(&leaves)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(batch_id: u8, tx_count: u32) -> BatchRefV1 {
        BatchRefV1 { batch_id: [batch_id; 32], certificate_hash: [0u8; 32], worker: WorkerId(0), tx_count }
    }

    #[test]
    fn empty_set_root_is_zero() {
        assert_eq!(batch_set_root(&BatchSetV1::default()), [0u8; 32]);
    }

    #[test]
    fn root_changes_when_a_ref_is_added() {
        let mut set = BatchSetV1 { refs: vec![r(1, 100)] };
        let root1 = batch_set_root(&set);
        set.refs.push(r(2, 200));
        let root2 = batch_set_root(&set);
        assert_ne!(root1, root2);
    }

    #[test]
    fn root_changes_when_ref_order_changes() {
        let a = BatchSetV1 { refs: vec![r(1, 10), r(2, 20)] };
        let b = BatchSetV1 { refs: vec![r(2, 20), r(1, 10)] };
        assert_ne!(batch_set_root(&a), batch_set_root(&b), "order must be part of the commitment, same as tx_root's own order-sensitivity");
    }

    #[test]
    fn root_changes_when_tx_count_changes_but_batch_id_does_not() {
        // Two refs pointing at the same batch_id but claiming a different
        // tx_count must NOT collide -- a header lying about how many ops a
        // committed batch carries would be a real integrity gap.
        let a = BatchSetV1 { refs: vec![r(1, 10)] };
        let b = BatchSetV1 { refs: vec![BatchRefV1 { tx_count: 999, ..r(1, 10) }] };
        assert_ne!(batch_set_root(&a), batch_set_root(&b));
    }

    #[test]
    fn total_tx_count_sums_every_ref() {
        let set = BatchSetV1 { refs: vec![r(1, 10), r(2, 20), r(3, 30)] };
        assert_eq!(set.total_tx_count(), 60);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn header_stays_one_hash_regardless_of_how_many_batches_are_committed() {
        // The whole point (§12): a block can commit to THOUSANDS of batches
        // by referencing one root. This test doesn't measure byte size
        // directly (batch_set_root's return type is fixed at [u8;32] by
        // construction) -- it instead proves the root is still a SINGLE,
        // well-defined value even for a set two orders of magnitude larger
        // than the earlier tests, so "header stays constant-size" isn't just
        // true by type signature but genuinely computable at that scale.
        let big_set = BatchSetV1 { refs: (0..5_000u32).map(|i| r((i % 256) as u8, i)).collect() };
        let root = batch_set_root(&big_set);
        assert_ne!(root, [0u8; 32]);
        assert_eq!(root, batch_set_root(&big_set), "must remain deterministic even at this scale");
    }
}
