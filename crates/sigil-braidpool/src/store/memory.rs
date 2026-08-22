//! batch_store.rs — Phase C, "local batches" (SIGIL_BRAIDPOOL_v1_1.md §17):
//! holds sealed batches keyed by `batch_id`, with the metrics counters
//! §23 asks for tracked as plain atomics (no Prometheus wiring yet — that's
//! a real integration point for whenever this crate is actually deployed,
//! not something to fake here).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use crate::canonical::BatchHeaderV1;
use crate::types::WorkerBatch;

/// Snapshot of the store's counters — `sigil_braidpool_batches_sealed_total`
/// / `_batch_bytes` from §23, without committing to a specific metrics
/// library. `uncompressed_len` sums `BatchHeaderV1::uncompressed_len` — the
/// batch's own byte-size estimate at seal time, not the store's in-memory
/// footprint (which would also include the `HashMap`'s own overhead).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchStoreMetrics {
    pub sealed_total: u64,
    pub bytes_total: u64,
    pub live_count: usize,
}

/// Sealed batches, keyed by `batch_id`. `insert`/`get`/`remove` all take
/// `&self` (RwLock inside) so this composes with the rest of the crate's
/// `Arc<...>`-shared, lock-internal style.
pub struct BatchStore {
    batches: RwLock<HashMap<[u8; 32], WorkerBatch>>,
    sealed_total: AtomicU64,
    bytes_total: AtomicU64,
}

impl Default for BatchStore {
    fn default() -> Self {
        Self { batches: RwLock::new(HashMap::new()), sealed_total: AtomicU64::new(0), bytes_total: AtomicU64::new(0) }
    }
}

impl BatchStore {
    pub fn new() -> Self { Self::default() }

    /// Insert a freshly-sealed batch. `sealed_total`/`bytes_total` are
    /// lifetime counters (never decremented by `remove`) — they answer "how
    /// much work has this store done," not "how much is currently live"
    /// (that's `live_count`, from `len()`).
    pub fn insert(&self, header: &BatchHeaderV1, batch: WorkerBatch) {
        let id = header.batch_id();
        self.sealed_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_total.fetch_add(header.uncompressed_len as u64, Ordering::Relaxed);
        self.batches.write().unwrap().insert(id, batch);
    }

    pub fn get(&self, id: &[u8; 32]) -> Option<WorkerBatch> {
        self.batches.read().unwrap().get(id).cloned()
    }

    pub fn contains(&self, id: &[u8; 32]) -> bool {
        self.batches.read().unwrap().contains_key(id)
    }

    /// Remove and return a batch — e.g. once its containing block is
    /// finalized and the batch's own transactions have been applied, per
    /// design doc §13's `CERTIFIED -> RECONSTRUCTED` -> eventually GC'd
    /// lifecycle (that later lifecycle logic isn't built yet; this is just
    /// the storage primitive it'll sit on top of).
    pub fn remove(&self, id: &[u8; 32]) -> Option<WorkerBatch> {
        self.batches.write().unwrap().remove(id)
    }

    pub fn len(&self) -> usize { self.batches.read().unwrap().len() }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn metrics(&self) -> BatchStoreMetrics {
        BatchStoreMetrics {
            sealed_total: self.sealed_total.load(Ordering::Relaxed),
            bytes_total: self.bytes_total.load(Ordering::Relaxed),
            live_count: self.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{BatchSealer, SealPolicy};
    use crate::types::WorkerId;
    use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};
    use std::time::Duration;

    fn tx(amount: u128) -> sigil_tx::SignedTx {
        let (sk, pk, wallet) = ed25519_keygen();
        let t = SigilTx::Send { from: wallet, to: [1u8; 32], amount, token: [0u8; 32], fee: 0 };
        ed25519_sign_tx(t, &sk, &pk)
    }

    fn sealer() -> BatchSealer {
        let policy = SealPolicy { target_bytes: usize::MAX, target_txs: 1, max_latency: Duration::from_secs(3600) };
        BatchSealer::new(WorkerId(0), [0u8; 32], 0, policy)
    }

    #[test]
    fn insert_then_get_roundtrips() {
        let store = BatchStore::new();
        let s = sealer();
        s.push(vec![tx(1)]);
        let (header, batch) = s.try_seal(false).unwrap();
        let id = header.batch_id();
        store.insert(&header, batch.clone());
        let got = store.get(&id).expect("must find the batch just inserted");
        assert_eq!(got.txs.len(), batch.txs.len());
        assert!(store.contains(&id));
    }

    #[test]
    fn metrics_track_lifetime_totals_not_just_current_size() {
        let store = BatchStore::new();
        let s = sealer();

        s.push(vec![tx(1)]);
        let (h1, b1) = s.try_seal(false).unwrap();
        store.insert(&h1, b1);

        s.push(vec![tx(2)]);
        let (h2, b2) = s.try_seal(false).unwrap();
        let id2 = h2.batch_id();
        store.insert(&h2, b2);

        let m = store.metrics();
        assert_eq!(m.sealed_total, 2);
        assert_eq!(m.live_count, 2);
        assert!(m.bytes_total > 0);

        store.remove(&id2);
        let m2 = store.metrics();
        assert_eq!(m2.live_count, 1, "live_count must drop after remove");
        assert_eq!(m2.sealed_total, 2, "sealed_total is a lifetime counter — remove must not decrement it");
    }

    #[test]
    fn remove_returns_the_batch_and_it_is_no_longer_findable() {
        let store = BatchStore::new();
        let s = sealer();
        s.push(vec![tx(1)]);
        let (header, batch) = s.try_seal(false).unwrap();
        let id = header.batch_id();
        store.insert(&header, batch);
        let removed = store.remove(&id);
        assert!(removed.is_some());
        assert!(store.get(&id).is_none());
        assert!(!store.contains(&id));
    }

    #[test]
    fn missing_id_returns_none_not_a_panic() {
        let store = BatchStore::new();
        assert!(store.get(&[9u8; 32]).is_none());
        assert!(store.remove(&[9u8; 32]).is_none());
    }
}
