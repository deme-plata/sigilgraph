//! backend.rs — `MempoolBackend`: the ONE shared mempool object `sigil-node`'s
//! producer loop AND `sigil-api`'s money-API handlers both hold, so it is
//! structurally impossible for the two to diverge onto different mempools.
//!
//! Built to close a real correctness hazard found while wiring
//! `SIGIL_BRAIDPOOL=1` for Phase B (SIGIL_BRAIDPOOL_v1_1.md): before this,
//! `sigil_api::AppState` and `sigil-node`'s block-body `pull()` each held
//! their OWN `Arc<Mutex<sigil_tx::Mempool>>` — the SAME `Arc` today, by
//! convention, but nothing stopped a future change from swapping only one of
//! them behind a flag. If that happened, real user transactions submitted via
//! `/v1/transactions` would land in a mempool nobody ever pulls from — a
//! silent drop, not merely a slowdown. Both crates now hold `Arc<MempoolBackend>`
//! instead, and there is only one place (`from_env`) where the backend choice
//! is made.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use sigil_tx::{batch_auth_message, AuthorizedBatch, SignedTx, TxApplyError};

use crate::worker::{BoundedIngestResult, ShardedMempool};

/// The plain-tx ingestion path — the ONLY thing `SIGIL_BRAIDPOOL` actually
/// switches. The batch lane below is deliberately NOT part of this choice:
/// `AuthorizedBatch` submissions are single-author, low-volume relative to
/// plain txs, and don't benefit from wallet-sharded parallelism the way many
/// independent senders' txs do — there's no reason to duplicate it per backend.
enum TxBackend {
    Legacy(Mutex<sigil_tx::Mempool>),
    Sharded(ShardedMempool),
}

/// The `AuthorizedBatch` lane. Reuses `sigil_tx`'s own crypto as-is
/// (`AuthorizedBatch::verify()`, `batch_auth_message`) — only the
/// storage/dedup bookkeeping is duplicated here, and it's the same handful of
/// lines `sigil_tx::Mempool` already uses internally for its own `batches`/
/// `seen_batches`/`batch_ops_total` fields (crates/sigil-tx/src/lib.rs
/// `ingest_batch`/`pull_batches`/`batch_count`/`pending_batch_ops`).
#[derive(Default)]
struct BatchLane {
    batches: VecDeque<AuthorizedBatch>,
    seen_batches: HashSet<[u8; 32]>,
    batch_ops_total: u64,
}

impl BatchLane {
    fn ingest_batch(&mut self, batch: AuthorizedBatch) -> Result<usize, TxApplyError> {
        batch.verify()?;
        let key = batch_auth_message(&batch.author, batch.nonce, &batch.ops);
        if !self.seen_batches.insert(key) {
            return Err(TxApplyError::DuplicateBatch);
        }
        let ops = batch.ops.len();
        self.batch_ops_total += ops as u64;
        self.batches.push_back(batch);
        Ok(ops)
    }

    fn pull_batches(&mut self, max_ops: usize) -> Vec<AuthorizedBatch> {
        let mut out = Vec::new();
        let mut ops = 0usize;
        while let Some(front) = self.batches.front() {
            ops += front.ops.len();
            out.push(self.batches.pop_front().unwrap());
            if ops >= max_ops {
                break;
            }
        }
        out
    }

    fn batch_count(&self) -> usize { self.batches.len() }
    fn pending_batch_ops(&self) -> usize { self.batches.iter().map(|b| b.ops.len()).sum() }
}

/// The single shared mempool object. `Arc<MempoolBackend>` replaces
/// `Arc<Mutex<sigil_tx::Mempool>>` at every call site in `sigil-node` and
/// `sigil-api` — there is exactly one instance in the whole process, and both
/// crates hold the SAME `Arc`, same as before this change, but now there is
/// no seam where they could hold different mempools.
pub struct MempoolBackend {
    tx: TxBackend,
    batches: Mutex<BatchLane>,
}

impl MempoolBackend {
    /// Reads `SIGIL_BRAIDPOOL` (unset or anything but `"1"` -> legacy single
    /// mutex — BYTE-FOR-BYTE the same default behavior as before this type
    /// existed) and `SIGIL_BRAIDPOOL_WORKERS` (default: available core count).
    pub fn from_env() -> Self {
        let sharded = std::env::var("SIGIL_BRAIDPOOL").map(|v| v == "1").unwrap_or(false);
        let tx = if sharded {
            let workers: u16 = std::env::var("SIGIL_BRAIDPOOL_WORKERS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get() as u16).unwrap_or(4));
            eprintln!("\u{1f578} SIGIL_BRAIDPOOL=1 \u{2014} sharded mempool ACTIVE ({workers} workers)");
            TxBackend::Sharded(ShardedMempool::new(workers, [0u8; 32]))
        } else {
            TxBackend::Legacy(Mutex::new(sigil_tx::Mempool::new()))
        };
        Self { tx, batches: Mutex::new(BatchLane::default()) }
    }

    /// Explicit constructors for tests (no env dependence).
    pub fn legacy() -> Self {
        Self { tx: TxBackend::Legacy(Mutex::new(sigil_tx::Mempool::new())), batches: Mutex::new(BatchLane::default()) }
    }
    pub fn sharded(workers: u16) -> Self {
        Self { tx: TxBackend::Sharded(ShardedMempool::new(workers, [0u8; 32])), batches: Mutex::new(BatchLane::default()) }
    }

    pub fn is_sharded(&self) -> bool { matches!(self.tx, TxBackend::Sharded(_)) }

    // ── plain-tx path (the one SIGIL_BRAIDPOOL switches) ──────────────────

    pub fn ingest(&self, txs: Vec<SignedTx>) -> BoundedIngestResult {
        match &self.tx {
            TxBackend::Legacy(m) => {
                let r = m.lock().unwrap().ingest(txs);
                BoundedIngestResult { accepted: r.accepted, invalid: r.invalid, dupe: r.dupe, rejected_capacity: 0 }
            }
            TxBackend::Sharded(s) => s.ingest(txs),
        }
    }

    pub fn pull(&self, max: usize) -> Vec<SignedTx> {
        match &self.tx {
            TxBackend::Legacy(m) => m.lock().unwrap().pull(max),
            TxBackend::Sharded(s) => s.pull(max),
        }
    }

    pub fn len(&self) -> usize {
        match &self.tx {
            TxBackend::Legacy(m) => m.lock().unwrap().len(),
            TxBackend::Sharded(s) => s.total_len(),
        }
    }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn contains(&self, hash: &[u8; 32]) -> bool {
        match &self.tx {
            TxBackend::Legacy(m) => m.lock().unwrap().contains(hash),
            TxBackend::Sharded(s) => s.contains(hash),
        }
    }

    // ── batch lane (always the same, regardless of tx backend) ────────────

    pub fn ingest_batch(&self, batch: AuthorizedBatch) -> Result<usize, TxApplyError> {
        self.batches.lock().unwrap().ingest_batch(batch)
    }
    pub fn pull_batches(&self, max_ops: usize) -> Vec<AuthorizedBatch> {
        self.batches.lock().unwrap().pull_batches(max_ops)
    }
    pub fn batch_count(&self) -> usize { self.batches.lock().unwrap().batch_count() }
    pub fn pending_batch_ops(&self) -> usize { self.batches.lock().unwrap().pending_batch_ops() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

    fn tx(sk: &[u8; 32], pk: &[u8; 32], wallet: sigil_state::WalletId, amount: u128) -> SignedTx {
        let t = SigilTx::Send { from: wallet, to: [1u8; 32], amount, token: [0u8; 32], fee: 0 };
        ed25519_sign_tx(t, sk, pk)
    }

    #[test]
    fn default_from_env_is_legacy_when_unset() {
        std::env::remove_var("SIGIL_BRAIDPOOL");
        let b = MempoolBackend::from_env();
        assert!(!b.is_sharded(), "unset SIGIL_BRAIDPOOL must default to the legacy backend — no behavior change for anyone who doesn't opt in");
    }

    #[test]
    fn legacy_and_sharded_both_implement_the_full_plain_tx_surface_identically() {
        for backend in [MempoolBackend::legacy(), MempoolBackend::sharded(4)] {
            let (sk, pk, wallet) = ed25519_keygen();
            let t = tx(&sk, &pk, wallet, 1);
            let r = backend.ingest(vec![t.clone()]);
            assert_eq!(r.accepted, 1);
            assert_eq!(backend.len(), 1);
            assert!(backend.contains(&t.tx.hash()));
            let pulled = backend.pull(10);
            assert_eq!(pulled.len(), 1);
            assert!(backend.is_empty());
        }
    }

    #[test]
    fn batch_lane_works_identically_regardless_of_tx_backend() {
        for backend in [MempoolBackend::legacy(), MempoolBackend::sharded(4)] {
            let (sk, pk, author) = ed25519_keygen();
            let ops = vec![SigilTx::Send { from: author, to: [2u8; 32], amount: 5, token: [0u8; 32], fee: 0 }];
            let batch = AuthorizedBatch::sign_ed25519(ops, 1, &sk, &pk);
            let accepted_ops = backend.ingest_batch(batch.clone()).expect("first ingest must succeed");
            assert_eq!(accepted_ops, 1);
            assert_eq!(backend.batch_count(), 1);
            assert_eq!(backend.pending_batch_ops(), 1);
            // replay must be rejected regardless of which tx backend is active —
            // this is exactly the "the two must never diverge" property this
            // type exists to guarantee.
            assert!(backend.ingest_batch(batch).is_err());
            let pulled = backend.pull_batches(100);
            assert_eq!(pulled.len(), 1);
            assert_eq!(backend.batch_count(), 0);
        }
    }

    /// The hazard this type was built to close, stated as a test: a tx
    /// ingested through the SAME `MempoolBackend` handle must be pull-able
    /// through that SAME handle — there is no second, divergent mempool it
    /// could have silently landed in instead.
    #[test]
    fn ingested_tx_is_always_reachable_via_the_same_handle() {
        for backend in [MempoolBackend::legacy(), MempoolBackend::sharded(4)] {
            let (sk, pk, wallet) = ed25519_keygen();
            let t = tx(&sk, &pk, wallet, 42);
            let hash = t.tx.hash();
            backend.ingest(vec![t]);
            assert!(backend.contains(&hash));
            let pulled = backend.pull(usize::MAX);
            assert!(pulled.iter().any(|p| p.tx.hash() == hash), "the ingested tx must be reachable via pull() on the SAME handle it was ingested through");
        }
    }
}
