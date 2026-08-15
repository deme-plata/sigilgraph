//! worker.rs — sharded, parallel-lock ingestion (design doc §3.1).
//!
//! Today's `sigil_tx::Mempool` is one `Mutex` shared by the whole node. This
//! splits ingestion across N independent lock domains, sharded by wallet, so
//! unrelated senders' transactions verify and enqueue with zero contention.
//! Same-wallet transactions always land in the same worker, preserving the
//! per-wallet ordering `SigilState::check_and_bump_nonce` needs at apply time
//! without any cross-worker coordination.

use parking_lot::Mutex;
use sigil_state::WalletId;
use sigil_tx::{verify_partition_parallel, MempoolIngest, SignedTx};
use std::collections::{HashSet, VecDeque};

use crate::types::WorkerId;

/// FNV-1a 64-bit — fast, deterministic, good-enough distribution for sharding
/// (not a security boundary; the dedup `HashSet` and signature checks are).
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

struct WorkerInner {
    verified: VecDeque<SignedTx>,
    seen: HashSet<[u8; 32]>,
    verified_total: u64,
}

impl Default for WorkerInner {
    fn default() -> Self {
        Self { verified: VecDeque::new(), seen: HashSet::new(), verified_total: 0 }
    }
}

/// One independent ingestion lane. Verification (`verify_partition_parallel`,
/// which itself uses ed25519-dalek's batched multi-signature verify — see
/// sigil-tx's doc comment on that function) happens OUTSIDE any lock; only the
/// final enqueue is serialized, so the lock is held for a memcpy, not a crypto
/// operation.
pub struct MempoolWorker {
    pub id: WorkerId,
    inner: Mutex<WorkerInner>,
}

impl MempoolWorker {
    fn new(id: WorkerId) -> Self {
        Self { id, inner: Mutex::new(WorkerInner::default()) }
    }

    /// Dedup + batch-verify a set of txs ALREADY ROUTED to this worker (see
    /// [`ShardedMempool::ingest`] for routing), then enqueue the valid ones.
    fn ingest_routed(&self, txs: Vec<SignedTx>) -> MempoolIngest {
        // Dedup check happens with the lock held only long enough to filter —
        // no crypto under the lock. Checks BOTH against previously-committed
        // hashes (`inner.seen`) AND within this same batch (`in_this_call`) —
        // the same tx appearing twice in one `txs` Vec must count as one
        // accept + one dupe, not two accepts, even though neither copy is in
        // `inner.seen` yet when the loop starts.
        let (fresh, dupe): (Vec<SignedTx>, usize) = {
            let inner = self.inner.lock();
            let mut fresh = Vec::with_capacity(txs.len());
            let mut dupe = 0usize;
            let mut in_this_call = HashSet::new();
            for t in txs {
                let h = t.tx.hash();
                if inner.seen.contains(&h) || !in_this_call.insert(h) {
                    dupe += 1;
                } else {
                    fresh.push(t);
                }
            }
            (fresh, dupe)
        };
        // The expensive part — batched signature verification — runs with NO
        // lock held at all, so other workers (and even other callers into
        // THIS worker for a disjoint tx set) are never blocked by it.
        let (valid, invalid) = verify_partition_parallel(fresh);
        let accepted = valid.len();
        let mut inner = self.inner.lock();
        for t in &valid { inner.seen.insert(t.tx.hash()); }
        inner.verified_total += accepted as u64;
        inner.verified.extend(valid);
        MempoolIngest { accepted, invalid: invalid.len(), dupe }
    }

    pub fn pull(&self, max: usize) -> Vec<SignedTx> {
        let mut inner = self.inner.lock();
        let n = max.min(inner.verified.len());
        inner.verified.drain(..n).collect()
    }

    pub fn len(&self) -> usize { self.inner.lock().verified.len() }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn verified_total(&self) -> u64 { self.inner.lock().verified_total }
    pub fn contains(&self, hash: &[u8; 32]) -> bool { self.inner.lock().seen.contains(hash) }
}

/// N independent [`MempoolWorker`]s, routed by `fee_payer` wallet hash. This is
/// the drop-in each call site in `sigil-node/src/main.rs` swaps to in Phase 1
/// (design doc §5) — same `ingest`/`pull`/`len` shape as today's
/// `sigil_tx::Mempool`, so the swap is mechanical once benchmarked.
pub struct ShardedMempool {
    workers: Vec<MempoolWorker>,
}

impl ShardedMempool {
    /// `worker_count` should track available cores in production; tests use
    /// small counts to keep sharding behavior easy to reason about.
    pub fn new(worker_count: u16) -> Self {
        let worker_count = worker_count.max(1);
        let workers = (0..worker_count).map(|i| MempoolWorker::new(WorkerId(i))).collect();
        Self { workers }
    }

    pub fn worker_count(&self) -> usize { self.workers.len() }

    fn worker_index_for(&self, wallet: &WalletId) -> usize {
        (fnv1a64(wallet) % self.workers.len() as u64) as usize
    }

    pub fn worker_for(&self, wallet: &WalletId) -> &MempoolWorker {
        &self.workers[self.worker_index_for(wallet)]
    }

    /// Route each tx to its wallet's worker (grouping first so each worker's
    /// batched verify runs over its whole share in one call, not tx-by-tx),
    /// then ingest each group. Groups run independently — a real caller can
    /// parallelize this loop with e.g. `rayon` across workers; kept sequential
    /// here so the crate has zero required async/thread-pool dependency for
    /// Phase 0 (the design doc's own throughput claim is about lock removal +
    /// batched verify, not about this call site itself being parallel yet).
    pub fn ingest(&self, txs: Vec<SignedTx>) -> MempoolIngest {
        let mut by_worker: Vec<Vec<SignedTx>> = (0..self.workers.len()).map(|_| Vec::new()).collect();
        for t in txs {
            let idx = self.worker_index_for(&t.tx.fee_payer());
            by_worker[idx].push(t);
        }
        let mut total = MempoolIngest { accepted: 0, invalid: 0, dupe: 0 };
        for (idx, group) in by_worker.into_iter().enumerate() {
            if group.is_empty() { continue; }
            let r = self.workers[idx].ingest_routed(group);
            total.accepted += r.accepted;
            total.invalid += r.invalid;
            total.dupe += r.dupe;
        }
        total
    }

    /// Pull up to `max` total, round-robin across workers so no single busy
    /// wallet-shard starves the others out of a block.
    pub fn pull(&self, max: usize) -> Vec<SignedTx> {
        let mut out = Vec::with_capacity(max.min(self.total_len()));
        let mut remaining = max;
        // Two passes: first pass gives each worker a fair `max/N` share; a
        // second pass mops up any leftover budget from workers that had less
        // than their share queued, so `max` is still honored when total
        // demand allows it.
        let per_worker = (remaining / self.workers.len().max(1)).max(1);
        for w in &self.workers {
            if remaining == 0 { break; }
            let take = per_worker.min(remaining);
            let got = w.pull(take);
            remaining -= got.len();
            out.extend(got);
        }
        if remaining > 0 {
            for w in &self.workers {
                if remaining == 0 { break; }
                let got = w.pull(remaining);
                remaining -= got.len();
                out.extend(got);
            }
        }
        out
    }

    pub fn total_len(&self) -> usize { self.workers.iter().map(|w| w.len()).sum() }
    pub fn is_empty(&self) -> bool { self.total_len() == 0 }
    pub fn verified_total(&self) -> u64 { self.workers.iter().map(|w| w.verified_total()).sum() }
    pub fn contains(&self, hash: &[u8; 32]) -> bool { self.workers.iter().any(|w| w.contains(hash)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_tx::{ed25519_keygen, ed25519_sign_tx, SigilTx};

    fn tx_from(wallet_sk: &[u8; 32], wallet_pk: &[u8; 32], wallet: WalletId, amount: u128) -> SignedTx {
        let tx = SigilTx::Send { from: wallet, to: [7u8; 32], amount, token: [0u8; 32], fee: 0 };
        ed25519_sign_tx(tx, wallet_sk, wallet_pk)
    }

    #[test]
    fn same_wallet_always_routes_to_the_same_worker() {
        let mempool = ShardedMempool::new(8);
        let (sk, pk, wallet) = ed25519_keygen();
        let idx1 = mempool.worker_index_for(&wallet);
        let idx2 = mempool.worker_index_for(&wallet);
        assert_eq!(idx1, idx2, "routing must be a pure function of the wallet");
        // and it must match the worker actually used by ingest():
        let tx = tx_from(&sk, &pk, wallet, 1);
        let r = mempool.ingest(vec![tx.clone()]);
        assert_eq!(r.accepted, 1);
        assert!(mempool.workers[idx1].contains(&tx.tx.hash()));
    }

    #[test]
    fn different_wallets_spread_across_workers() {
        let mempool = ShardedMempool::new(8);
        let mut hit: HashSet<usize> = HashSet::new();
        for _ in 0..64 {
            let (_sk, _pk, wallet) = ed25519_keygen();
            hit.insert(mempool.worker_index_for(&wallet));
        }
        // 64 random wallets across 8 workers should touch more than one
        // worker essentially always — this is a distribution sanity check,
        // not a cryptographic uniformity proof.
        assert!(hit.len() > 1, "64 random wallets landed on a single worker — sharding is broken");
    }

    #[test]
    fn ingest_dedups_within_and_across_calls() {
        let mempool = ShardedMempool::new(4);
        let (sk, pk, wallet) = ed25519_keygen();
        let tx = tx_from(&sk, &pk, wallet, 0);

        let r1 = mempool.ingest(vec![tx.clone(), tx.clone()]);
        assert_eq!(r1.accepted, 1);
        assert_eq!(r1.dupe, 1, "duplicate within the same call must be caught");

        let r2 = mempool.ingest(vec![tx.clone()]);
        assert_eq!(r2.accepted, 0);
        assert_eq!(r2.dupe, 1, "duplicate across calls must still be caught");
        assert_eq!(mempool.total_len(), 1);
    }

    #[test]
    fn ingest_rejects_invalid_signature() {
        let mempool = ShardedMempool::new(4);
        let (sk, pk, wallet) = ed25519_keygen();
        let mut tx = tx_from(&sk, &pk, wallet, 0);
        tx.sig.0[0] ^= 0xff; // corrupt the signature
        let r = mempool.ingest(vec![tx]);
        assert_eq!(r.accepted, 0);
        assert_eq!(r.invalid, 1);
    }

    #[test]
    fn pull_returns_everything_queued_across_all_workers() {
        let mempool = ShardedMempool::new(4);
        let mut expected = HashSet::new();
        for i in 0..20u64 {
            let (sk, pk, wallet) = ed25519_keygen();
            let tx = tx_from(&sk, &pk, wallet, i as u128);
            expected.insert(tx.tx.hash());
            mempool.ingest(vec![tx]);
        }
        assert_eq!(mempool.total_len(), 20);
        let pulled = mempool.pull(100);
        assert_eq!(pulled.len(), 20, "pull(100) must drain all 20 queued txs");
        let pulled_hashes: HashSet<_> = pulled.iter().map(|t| t.tx.hash()).collect();
        assert_eq!(pulled_hashes, expected);
        assert!(mempool.is_empty());
    }

    #[test]
    fn pull_respects_the_max_budget() {
        let mempool = ShardedMempool::new(4);
        for i in 0..20u64 {
            let (sk, pk, wallet) = ed25519_keygen();
            mempool.ingest(vec![tx_from(&sk, &pk, wallet, i as u128)]);
        }
        let pulled = mempool.pull(7);
        assert_eq!(pulled.len(), 7);
        assert_eq!(mempool.total_len(), 13);
    }
}
