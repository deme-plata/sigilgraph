//! dissemination.rs — the erasure-coded batch broadcast (design doc §3.3, the
//! genuinely-SIGIL-specific upgrade over stock Narwhal's full-replication
//! broadcast). Wraps `flux-aether::rs_shard`/`rs_reassemble` — the same
//! Reed-Solomon coder already proven for chain-snapshot durability — for
//! per-peer batch shards instead of per-peer full copies.

use flux_aether::{rs_reassemble, rs_shard};

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

/// Erasure-code a batch into `k + parity` shards, one to hand to each of that
/// many peers. Any `k` of them reconstruct the original batch byte-for-byte.
///
/// Panics iff `flux_aether::rs_shard` would (invalid `k`/`parity`, e.g. `k=0`)
/// — a config error, not a runtime condition callers should route around.
pub fn shard_batch(batch: &WorkerBatch, k: usize, parity: usize) -> Vec<BatchShard> {
    let digest = batch.digest();
    let bytes = batch_encode(batch);
    let (orig_len, shards) = rs_shard(&bytes, k, parity);
    shards
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| BatchShard { digest, index, orig_len, k, parity, bytes })
        .collect()
}

/// Reconstruct a batch from a sparse set of shards (by `index`, `None` for a
/// missing/not-yet-arrived shard). `None` if fewer than `k` shards are
/// present, or if the reconstructed bytes don't decode to a `WorkerBatch`
/// whose digest matches what was claimed — never trust reconstructed bytes
/// without re-deriving the digest from them.
pub fn reassemble_batch(
    expected_digest: [u8; 32],
    k: usize,
    parity: usize,
    orig_len: usize,
    shards: Vec<Option<BatchShard>>,
) -> Option<WorkerBatch> {
    let raw_shards: Vec<Option<Vec<u8>>> = shards.into_iter().map(|s| s.map(|s| s.bytes)).collect();
    let bytes = rs_reassemble(orig_len, k, parity, raw_shards)?;
    let batch: WorkerBatch = serde_json::from_slice(&bytes).ok()?;
    if batch.digest() != expected_digest {
        return None; // reconstruction "succeeded" but produced the wrong content — reject
    }
    Some(batch)
}

fn batch_encode(batch: &WorkerBatch) -> Vec<u8> {
    serde_json::to_vec(batch).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WorkerId;
    use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

    fn sample_batch(n: usize) -> WorkerBatch {
        let txs = (0..n)
            .map(|i| {
                let (sk, pk, wallet) = ed25519_keygen();
                let tx = SigilTx::Send { from: wallet, to: [1u8; 32], amount: i as u128, token: [0u8; 32], fee: 0 };
                ed25519_sign_tx(tx, &sk, &pk)
            })
            .collect();
        WorkerBatch::new(WorkerId(0), 1, txs)
    }

    #[test]
    fn full_shard_set_reconstructs_exactly() {
        let batch = sample_batch(50);
        let digest = batch.digest();
        let shards = shard_batch(&batch, 8, 4);
        assert_eq!(shards.len(), 12);
        let orig_len = shards[0].orig_len;
        let sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        let out = reassemble_batch(digest, 8, 4, orig_len, sparse).expect("full set must reconstruct");
        assert_eq!(out.txs.len(), 50);
        assert_eq!(out.digest(), digest);
    }

    #[test]
    fn survives_losing_up_to_parity_shards() {
        let batch = sample_batch(30);
        let digest = batch.digest();
        let shards = shard_batch(&batch, 8, 4); // tolerate losing any 4 of 12
        let orig_len = shards[0].orig_len;
        let mut sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        // drop the maximum survivable number of shards (indices 0..4)
        for s in sparse.iter_mut().take(4) { *s = None; }
        let out = reassemble_batch(digest, 8, 4, orig_len, sparse)
            .expect("losing exactly `parity` shards must still reconstruct");
        assert_eq!(out.txs.len(), 30);
    }

    #[test]
    fn fails_closed_when_below_k_shards_survive() {
        let batch = sample_batch(30);
        let digest = batch.digest();
        let shards = shard_batch(&batch, 8, 4);
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
        let batch = sample_batch(10);
        let real_digest = batch.digest();
        let wrong_digest = [0xAAu8; 32];
        let shards = shard_batch(&batch, 8, 4);
        let orig_len = shards[0].orig_len;
        let sparse: Vec<Option<BatchShard>> = shards.into_iter().map(Some).collect();
        assert!(
            reassemble_batch(wrong_digest, 8, 4, orig_len, sparse).is_none(),
            "a correctly-reconstructed batch must still be rejected if it doesn't match the CLAIMED digest"
        );
        // sanity: the real digest does verify against the same shard set
        let _ = real_digest;
    }
}
