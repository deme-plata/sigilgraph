//! dissemination.rs — the erasure-coded batch broadcast (design doc §3.3 —
//! NOT novel, see `SIGIL_NARWHAL_ARXIV_INVESTIGATION_v0.pdf`: Imitater
//! [arXiv:2409.19286] already erasure-codes shared-mempool microblocks this
//! way). Wraps `flux-aether::rs_shard`/`rs_reassemble` — the same
//! Reed-Solomon coder already proven for chain-snapshot durability — for
//! per-peer batch shards instead of per-peer full copies.
//!
//! CORRECTED 2026-08-15 (Phase E, wiring this up to what Phase C's sealer
//! actually produces): shards now carry the CANONICAL `BatchHeaderV1::batch_id()`
//! as their identity, not `WorkerBatch`'s old bare Phase-0 digest. The two
//! schemes were never meant to coexist as competing identities for the same
//! object — `batch_id()` is the one every other Phase A/C piece
//! (`BatchSealer`, `BatchStore`) actually uses. `shard_batch`/`reassemble_batch`
//! now shard the `(BatchHeaderV1, WorkerBatch)` PAIR together, so a
//! successful reconstruction recovers both — letting reassembly recompute
//! `header.batch_id()` from the actually-reconstructed bytes (not trust a
//! separately-transmitted header) AND independently re-derive `tx_root` from
//! the reconstructed transactions, matching Imitater's own
//! decode-then-re-encode-then-compare-commitment defense (adopted, not
//! invented — see the investigation PDF).

use flux_aether::{rs_reassemble, rs_shard};

use crate::canonical::BatchHeaderV1;
use crate::merkle::merkle_root;
use crate::types::WorkerBatch;

/// One peer's slice of an erasure-coded batch. `index` identifies which of
/// the `k + parity` shards this is — needed at reconstruction time since
/// shards can arrive out of order or with some missing.
#[derive(Debug, Clone)]
pub struct BatchShard {
    pub digest: [u8; 32],
    pub index: usize,
    pub orig_len: usize,
    pub k: usize,
    pub parity: usize,
    pub bytes: Vec<u8>,
}

/// Erasure-code a sealed `(header, batch)` pair into `k + parity` shards, one
/// to hand to each of that many peers. Any `k` of them reconstruct the
/// original bytes byte-for-byte.
///
/// Panics iff `flux_aether::rs_shard` would (invalid `k`/`parity`, e.g. `k=0`)
/// — a config error, not a runtime condition callers should route around.
pub fn shard_batch(header: &BatchHeaderV1, batch: &WorkerBatch, k: usize, parity: usize) -> Vec<BatchShard> {
    let digest = header.batch_id();
    let bytes = encode_pair(header, batch);
    let (orig_len, shards) = rs_shard(&bytes, k, parity);
    shards
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| BatchShard { digest, index, orig_len, k, parity, bytes })
        .collect()
}

/// Reconstruct `(header, batch)` from a sparse set of shards (by `index`,
/// `None` for a missing/not-yet-arrived shard). `None` if fewer than `k`
/// shards are present, if the reconstructed bytes don't decode, if the
/// decoded header's `batch_id()` doesn't match `expected_digest`, OR if the
/// decoded header's `tx_root` doesn't match a Merkle root RECOMPUTED from
/// the decoded transactions — two independent checks, so a bug that broke
/// only one of them wouldn't silently accept corrupt content.
pub fn reassemble_batch(
    expected_digest: [u8; 32],
    k: usize,
    parity: usize,
    orig_len: usize,
    shards: Vec<Option<BatchShard>>,
) -> Option<(BatchHeaderV1, WorkerBatch)> {
    let raw_shards: Vec<Option<Vec<u8>>> = shards.into_iter().map(|s| s.map(|s| s.bytes)).collect();
    let bytes = rs_reassemble(orig_len, k, parity, raw_shards)?;
    let (header, batch): (BatchHeaderV1, WorkerBatch) = serde_json::from_slice(&bytes).ok()?;
    if header.batch_id() != expected_digest {
        return None; // reconstruction "succeeded" but produced the wrong content — reject
    }
    let recomputed_tx_root = merkle_root(&batch.txs.iter().map(|t| t.tx.hash()).collect::<Vec<_>>());
    if recomputed_tx_root != header.tx_root {
        return None; // header and batch disagree — never trust the pairing on batch_id alone
    }
    Some((header, batch))
}

fn encode_pair(header: &BatchHeaderV1, batch: &WorkerBatch) -> Vec<u8> {
    serde_json::to_vec(&(header, batch)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WorkerId;
    use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

    fn sample(n: usize) -> (BatchHeaderV1, WorkerBatch) {
        let txs = (0..n)
            .map(|i| {
                let (sk, pk, wallet) = ed25519_keygen();
                let tx = SigilTx::Send { from: wallet, to: [1u8; 32], amount: i as u128, token: [0u8; 32], fee: 0 };
                ed25519_sign_tx(tx, &sk, &pk)
            })
            .collect();
        let batch = WorkerBatch::new(WorkerId(0), 1, txs);
        let header = batch.canonical_header([7u8; 32], 3, 1, None);
        (header, batch)
    }

    #[test]
    fn full_shard_set_reconstructs_exactly() {
        let (header, batch) = sample(50);
        let digest = header.batch_id();
        let shards = shard_batch(&header, &batch, 8, 4);
        assert_eq!(shards.len(), 12);
        let orig_len = shards[0].orig_len;
        let sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        let (out_header, out_batch) = reassemble_batch(digest, 8, 4, orig_len, sparse).expect("full set must reconstruct");
        assert_eq!(out_batch.txs.len(), 50);
        assert_eq!(out_header.batch_id(), digest);
    }

    #[test]
    fn survives_losing_up_to_parity_shards() {
        let (header, batch) = sample(30);
        let digest = header.batch_id();
        let shards = shard_batch(&header, &batch, 8, 4); // tolerate losing any 4 of 12
        let orig_len = shards[0].orig_len;
        let mut sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        // drop the maximum survivable number of shards (indices 0..4)
        for s in sparse.iter_mut().take(4) { *s = None; }
        let (_, out_batch) = reassemble_batch(digest, 8, 4, orig_len, sparse)
            .expect("losing exactly `parity` shards must still reconstruct");
        assert_eq!(out_batch.txs.len(), 30);
    }

    #[test]
    fn fails_closed_when_below_k_shards_survive() {
        let (header, batch) = sample(30);
        let digest = header.batch_id();
        let shards = shard_batch(&header, &batch, 8, 4);
        let orig_len = shards[0].orig_len;
        let mut sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        // drop ONE more than parity allows (5 of 12, k=8 needs 8 survivors, only 7 remain)
        for s in sparse.iter_mut().take(5) { *s = None; }
        assert!(
            reassemble_batch(digest, 8, 4, orig_len, sparse).is_none(),
            "losing more than `parity` shards must fail closed, not return corrupt data"
        );
    }

    #[test]
    fn rejects_reconstruction_against_the_wrong_expected_digest() {
        let (header, batch) = sample(10);
        let wrong_digest = [0xAAu8; 32];
        let shards = shard_batch(&header, &batch, 8, 4);
        let orig_len = shards[0].orig_len;
        let sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        assert!(
            reassemble_batch(wrong_digest, 8, 4, orig_len, sparse).is_none(),
            "a correctly-reconstructed batch must still be rejected if it doesn't match the CLAIMED digest"
        );
    }

    /// The defense-in-depth this pass added: even if a header's `batch_id()`
    /// SOMEHOW matched `expected_digest` (e.g. a hash-construction bug), a
    /// header/batch pairing whose `tx_root` doesn't match the ACTUAL
    /// reconstructed transactions must still be rejected. Constructed by
    /// hand-corrupting a header's `tx_root` after sealing, then re-sharding
    /// the (corrupted-header, real-batch) pair under the header's OWN
    /// (now-different) batch_id — proving the tx_root cross-check is doing
    /// real work independent of the batch_id check.
    #[test]
    fn rejects_when_tx_root_disagrees_with_reconstructed_txs() {
        let (mut header, batch) = sample(5);
        header.tx_root = [0xFFu8; 32]; // corrupt: no longer matches batch.txs
        let digest = header.batch_id(); // batch_id incorporates tx_root, so THIS is now self-consistent
        let shards = shard_batch(&header, &batch, 8, 4);
        let orig_len = shards[0].orig_len;
        let sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        assert!(
            reassemble_batch(digest, 8, 4, orig_len, sparse).is_none(),
            "batch_id matching is not enough if tx_root itself doesn't match the actual reconstructed txs"
        );
    }
}
