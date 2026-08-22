//! sealer.rs — Phase C, "local batches" (SIGIL_BRAIDPOOL_v1_1.md §17, §8):
//! turns verified transactions pulled from a [`crate::worker::ShardedMempool`]
//! worker into a sealed [`crate::types::WorkerBatch`] + its canonical
//! [`crate::canonical::BatchHeaderV1`], once a LOCAL sealing policy is met.
//!
//! "Deterministic by local policy, not consensus" (§8): the byte/tx-count/
//! latency thresholds below decide WHEN one node seals a batch, but that
//! moment does not need to match any other node's — only the resulting
//! `batch_id` (computed from content, not from when it was sealed) is
//! consensus-visible. Two nodes sealing the "same" pending transactions at
//! different wall-clock moments simply produce two different batches, which
//! is fine — nothing about correctness depends on sealing being synchronized.
//!
//! Deliberately NOT wired into `sigil-node`'s block body: blocks still carry
//! inline transactions (BraidPool's Phase C explicitly keeps it that way —
//! `BlockBatchRef`-referenced bodies are Phase F, gated on real multi-validator
//! availability per the design doc's §3.2 safety correction).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sigil_tx::SignedTx;

use crate::canonical::BatchHeaderV1;
use crate::types::WorkerId;

/// When to seal a batch. All three thresholds are OR'd — whichever fires
/// first triggers a seal (matches §8's `seal when: bytes >= target OR
/// tx_count >= target OR oldest_tx_age >= target`).
#[derive(Clone, Copy, Debug)]
pub struct SealPolicy {
    pub target_bytes: usize,
    pub target_txs: usize,
    pub max_latency: Duration,
}

impl Default for SealPolicy {
    fn default() -> Self {
        Self { target_bytes: 2 * 1024 * 1024, target_txs: 4_096, max_latency: Duration::from_millis(500) }
    }
}

/// Accumulates pulled transactions for ONE worker until a [`SealPolicy`]
/// threshold fires, then emits a sealed `(BatchHeaderV1, WorkerBatch)` pair.
/// Owns its own monotonic per-worker `sequence` counter — each batch this
/// sealer produces gets the next sequence number, which is part of what
/// makes its `batch_id` unique even across batches with identical content
/// (see `BatchHeaderV1`'s doc comment on why sequence is part of the id).
pub struct BatchSealer {
    worker: WorkerId,
    chain_id: [u8; 32],
    epoch: u64,
    policy: SealPolicy,
    sequence: AtomicU64,
    last_batch_id: Mutex<Option<[u8; 32]>>,
    pending: Mutex<PendingState>,
}

struct PendingState {
    txs: Vec<SignedTx>,
    bytes: usize,
    oldest: Option<Instant>,
}

impl Default for PendingState {
    fn default() -> Self {
        Self { txs: Vec::new(), bytes: 0, oldest: None }
    }
}

impl BatchSealer {
    pub fn new(worker: WorkerId, chain_id: [u8; 32], epoch: u64, policy: SealPolicy) -> Self {
        Self {
            worker,
            chain_id,
            epoch,
            policy,
            sequence: AtomicU64::new(0),
            last_batch_id: Mutex::new(None),
            pending: Mutex::new(PendingState::default()),
        }
    }

    /// Feed freshly-pulled txs in. Does NOT seal by itself — call
    /// [`Self::try_seal`] (e.g. on a timer or right after `push`) to actually
    /// check the policy and emit a batch. Split this way so a caller can push
    /// from one loop and poll for latency-based sealing from another without
    /// the two needing to be the same call.
    pub fn push(&self, txs: Vec<SignedTx>) {
        if txs.is_empty() {
            return;
        }
        let mut p = self.pending.lock().unwrap();
        if p.oldest.is_none() {
            p.oldest = Some(Instant::now());
        }
        for t in &txs {
            p.bytes += t.tx.encode().len();
        }
        p.txs.extend(txs);
    }

    /// Check the policy against current pending state; seal and return the
    /// batch if any threshold fired (or `force`, e.g. on shutdown so nothing
    /// pending is silently lost). `None` means: nothing to seal yet.
    pub fn try_seal(&self, force: bool) -> Option<(BatchHeaderV1, crate::types::WorkerBatch)> {
        let mut p = self.pending.lock().unwrap();
        if p.txs.is_empty() {
            return None;
        }
        let age_fired = p.oldest.map(|t| t.elapsed() >= self.policy.max_latency).unwrap_or(false);
        let fired = force
            || p.bytes >= self.policy.target_bytes
            || p.txs.len() >= self.policy.target_txs
            || age_fired;
        if !fired {
            return None;
        }

        let txs = std::mem::take(&mut p.txs);
        p.bytes = 0;
        p.oldest = None;
        drop(p);

        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let previous = *self.last_batch_id.lock().unwrap();
        let batch = crate::types::WorkerBatch::new(self.worker, sequence, txs);
        let header = batch.canonical_header(self.chain_id, self.epoch, sequence, previous);
        *self.last_batch_id.lock().unwrap() = Some(header.batch_id());
        Some((header, batch))
    }

    pub fn pending_len(&self) -> usize { self.pending.lock().unwrap().txs.len() }
    pub fn pending_bytes(&self) -> usize { self.pending.lock().unwrap().bytes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

    fn tx(amount: u128) -> SignedTx {
        let (sk, pk, wallet) = ed25519_keygen();
        let t = SigilTx::Send { from: wallet, to: [1u8; 32], amount, token: [0u8; 32], fee: 0 };
        ed25519_sign_tx(t, &sk, &pk)
    }

    #[test]
    fn does_not_seal_below_every_threshold() {
        let policy = SealPolicy { target_bytes: usize::MAX, target_txs: 100, max_latency: Duration::from_secs(3600) };
        let sealer = BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy);
        sealer.push(vec![tx(1), tx(2)]);
        assert!(sealer.try_seal(false).is_none(), "2 txs under a threshold of 100 must not seal");
        assert_eq!(sealer.pending_len(), 2);
    }

    #[test]
    fn seals_on_tx_count_threshold() {
        let policy = SealPolicy { target_bytes: usize::MAX, target_txs: 3, max_latency: Duration::from_secs(3600) };
        let sealer = BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy);
        sealer.push(vec![tx(1), tx(2), tx(3)]);
        let (header, batch) = sealer.try_seal(false).expect("3 txs must fire the tx_count=3 threshold");
        assert_eq!(batch.len(), 3);
        assert_eq!(header.tx_count, 3);
        assert_eq!(sealer.pending_len(), 0, "sealed txs must be drained from pending");
    }

    #[test]
    fn seals_on_byte_threshold() {
        let (sk, pk, wallet) = ed25519_keygen();
        let t = ed25519_sign_tx(
            SigilTx::Send { from: wallet, to: [1u8; 32], amount: 1, token: [0u8; 32], fee: 0 },
            &sk, &pk,
        );
        let one_tx_bytes = t.tx.encode().len();
        let policy = SealPolicy { target_bytes: one_tx_bytes, target_txs: usize::MAX, max_latency: Duration::from_secs(3600) };
        let sealer = BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy);
        sealer.push(vec![t]);
        assert!(sealer.try_seal(false).is_some(), "reaching target_bytes must fire regardless of tx count");
    }

    #[test]
    fn seals_on_latency_even_with_one_tx() {
        let policy = SealPolicy { target_bytes: usize::MAX, target_txs: usize::MAX, max_latency: Duration::from_millis(1) };
        let sealer = BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy);
        sealer.push(vec![tx(1)]);
        std::thread::sleep(Duration::from_millis(5));
        assert!(sealer.try_seal(false).is_some(), "a single tx sitting past max_latency must still seal — latency is a real bound, not just a fallback");
    }

    #[test]
    fn force_seals_even_when_nothing_would_otherwise_fire() {
        let policy = SealPolicy { target_bytes: usize::MAX, target_txs: usize::MAX, max_latency: Duration::from_secs(3600) };
        let sealer = BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy);
        sealer.push(vec![tx(1)]);
        assert!(sealer.try_seal(false).is_none());
        assert!(sealer.try_seal(true).is_some(), "force=true (e.g. on shutdown) must seal regardless of policy, so nothing pending is silently lost");
    }

    #[test]
    fn empty_pending_never_seals_even_when_forced() {
        let sealer = BatchSealer::new(WorkerId(0), [0u8; 32], 0, SealPolicy::default());
        assert!(sealer.try_seal(true).is_none(), "sealing an empty batch would be pointless and would still consume a sequence number");
    }

    #[test]
    fn consecutive_batches_get_increasing_sequence_and_chain_via_previous() {
        let policy = SealPolicy { target_bytes: usize::MAX, target_txs: 1, max_latency: Duration::from_secs(3600) };
        let sealer = BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy);

        sealer.push(vec![tx(1)]);
        let (h1, _) = sealer.try_seal(false).unwrap();
        assert_eq!(h1.sequence, 0);
        assert_eq!(h1.previous, None, "the first batch from a sealer has no previous");

        sealer.push(vec![tx(2)]);
        let (h2, _) = sealer.try_seal(false).unwrap();
        assert_eq!(h2.sequence, 1);
        assert_eq!(h2.previous, Some(h1.batch_id()), "each batch must chain to the sealer's previous batch id");
    }

    #[test]
    fn different_workers_produce_different_batch_ids_for_identical_content() {
        let policy = SealPolicy { target_bytes: usize::MAX, target_txs: 1, max_latency: Duration::from_secs(3600) };
        let a = BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy);
        let b = BatchSealer::new(WorkerId(1), [0u8; 32], 0, policy);
        let t = tx(1);
        a.push(vec![t.clone()]);
        b.push(vec![t]);
        let (ha, _) = a.try_seal(false).unwrap();
        let (hb, _) = b.try_seal(false).unwrap();
        assert_ne!(ha.batch_id(), hb.batch_id());
    }
}
