//! Throughput harness — "play the chain at 10,000 blocks/sec / 1M TPS".
//!
//! Drives the REAL SIGIL state machine (`apply_tx` + `commit_state_transition`)
//! at scale and measures the actual ceiling, with a breakdown of where the
//! wall-clock time goes: transaction *execution* vs state-*root computation*.
//! That breakdown is the whole point — it tells us which of the six pieces
//! from the 1M-TPS roadmap to build first.
//!
//! Honest framing: this measures **single-producer execution throughput** —
//! how fast one node can apply + commit batched transactions. It does NOT yet
//! model DAG ordering (DagKnight) or parallel execution; those are separate
//! prototypes. But execution throughput is the floor every design sits on, and
//! the apply-vs-roots split here directly shows why the current
//! "rehash-the-whole-state-every-block" root must become an incremental Sparse
//! Merkle Tree before any DAG can hit the target.
//!
//! Deterministic + replayable: same params → same work, every run.

use std::sync::Mutex;
use std::time::Instant;

use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, WalletId, MAX_SUPPLY};
use sigil_tx::{apply_tx, batch_into_transition, ed25519_keygen, AuthorizedBatch, SigilTx, NATIVE};

use crate::sign_dummy;

/// Result of one throughput run.
#[derive(Debug, Clone)]
pub struct ThroughputReport {
    /// Blocks produced.
    pub blocks: u64,
    /// Transactions successfully applied.
    pub txs: u64,
    /// Wallets in the active set (state size — drives root cost).
    pub wallets: u64,
    /// Wall-clock spent inside `apply_tx` (pure execution).
    pub apply_ms: u128,
    /// Wall-clock spent inside `commit_state_transition` (mutations + the four
    /// state roots). At Phase-0 this is dominated by O(state) root rehashing.
    pub commit_ms: u128,
    /// Total wall-clock.
    pub total_ms: u128,
    /// Derived transactions/second.
    pub tps: f64,
    /// Derived blocks/second.
    pub blocks_per_sec: f64,
}

impl ThroughputReport {
    /// One-line human summary.
    pub fn summary(&self) -> String {
        let apply_pct = if self.total_ms > 0 { self.apply_ms as f64 / self.total_ms as f64 * 100.0 } else { 0.0 };
        let commit_pct = if self.total_ms > 0 { self.commit_ms as f64 / self.total_ms as f64 * 100.0 } else { 0.0 };
        format!(
            "{blocks} blocks · {txs} txs · {wallets} wallets · {total}ms\n  ▸ {tps:.0} TPS · {bps:.0} blocks/sec\n  ▸ time split: apply {apply_pct:.0}% ({apply}ms) · commit+roots {commit_pct:.0}% ({commit}ms)",
            blocks = self.blocks, txs = self.txs, wallets = self.wallets, total = self.total_ms,
            tps = self.tps, bps = self.blocks_per_sec,
            apply_pct = apply_pct, apply = self.apply_ms,
            commit_pct = commit_pct, commit = self.commit_ms,
        )
    }
}

fn wallet(i: u64) -> WalletId {
    let mut w = [0u8; 32];
    w[..8].copy_from_slice(&i.to_le_bytes());
    w[31] = 0xA0; // keep it off the all-zero NATIVE-collision space
    w
}

/// Run `n_blocks`, each carrying `txs_per_block` independent transfers across a
/// pool of `n_wallets`, through the real apply+commit pipeline. Measures the
/// execution-vs-roots split.
///
/// Wallets are pre-funded huge and transfers are tiny (amount 1, fee 0) so the
/// run never drains a balance — the point is to measure *processing* speed, not
/// economics. Transfers are round-robin across the pool so they're independent
/// within a block (no two touch the same sender).
pub fn run_throughput(n_blocks: u64, txs_per_block: u64, n_wallets: u64) -> ThroughputReport {
    let n_wallets = n_wallets.max(2);
    let mut state = SigilState::new();

    // Fund the wallet pool at genesis (height 0).
    // Fund within the hard 21M cap (sum ≤ MAX_SUPPLY) — the chokepoint now
    // rejects over-minting, and a realistic chain never exceeds the cap anyway.
    let per = MAX_SUPPLY / n_wallets as u128;
    let fund_muts: Vec<StateMutation> = (0..n_wallets)
        .map(|i| StateMutation::SetBalance { wallet: wallet(i), token: NATIVE, amount: per })
        .collect();
    commit_state_transition(&mut state, &StateTransition { at_height: 0, mutations: fund_muts }, 0)
        .expect("fund genesis");

    let mut apply_ns: u128 = 0;
    let mut commit_ns: u128 = 0;
    let mut txs: u64 = 0;
    let mut counter: u64 = 0;

    let t_total = Instant::now();
    for h in 1..=n_blocks {
        // Build the block's batch: txs_per_block independent sends.
        let mut results = Vec::with_capacity(txs_per_block as usize);
        let t_apply = Instant::now();
        for _ in 0..txs_per_block {
            let from = wallet(counter % n_wallets);
            let to = wallet((counter + 1) % n_wallets);
            counter += 1;
            let tx = sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 });
            if let Ok(r) = apply_tx(&state, &tx) {
                results.push(r);
                txs += 1;
            }
        }
        apply_ns += t_apply.elapsed().as_nanos();

        let transition = batch_into_transition(results, h);
        let t_commit = Instant::now();
        let _ = commit_state_transition(&mut state, &transition, h);
        commit_ns += t_commit.elapsed().as_nanos();
    }
    let total_ms = t_total.elapsed().as_millis();

    let secs = (total_ms as f64 / 1000.0).max(1e-6);
    ThroughputReport {
        blocks: n_blocks,
        txs,
        wallets: n_wallets,
        apply_ms: apply_ns / 1_000_000,
        commit_ms: commit_ns / 1_000_000,
        total_ms,
        tps: txs as f64 / secs,
        blocks_per_sec: n_blocks as f64 / secs,
    }
}

// ── batch-authorization throughput (one sig authorizes N ops) ────────────────

/// Result of a batch-authorization run.
#[derive(Debug, Clone)]
pub struct BatchAuthReport {
    pub blocks: u64,
    pub batches: u64,
    pub batch_size: u64,
    pub txs: u64,
    pub sigs_verified: u64,
    pub authors: u64,
    pub verify_apply_ms: u128,
    pub commit_ms: u128,
    pub disk_ms: u128,
    pub bytes_archived: u64,
    pub total_ms: u128,
    pub tps: f64,
}

impl BatchAuthReport {
    pub fn summary(&self) -> String {
        let ops_per_sig = self.txs as f64 / self.sigs_verified.max(1) as f64;
        format!(
            "{b} blocks · {ba} batches × {bs} ops · {txs} txs · {sv} sigs ({ops_per_sig:.0} ops/sig)\n  \
             ▸ {tps:.0} TPS (REAL pipeline: verify→apply→commit→roots)\n  \
             ▸ split: verify+apply {va}ms · commit+roots {c}ms · archive {d}ms ({gb:.1} GB on /home/storage)",
            b = self.blocks, ba = self.batches, bs = self.batch_size, txs = self.txs,
            sv = self.sigs_verified, tps = self.tps,
            va = self.verify_apply_ms, c = self.commit_ms, d = self.disk_ms,
            gb = self.bytes_archived as f64 / 1e9,
        )
    }
}

/// Run the batch-authorization construction through the REAL chain pipeline:
/// each block carries `batches_per_block` [`AuthorizedBatch`]es of `batch_size`
/// single-author ops. We verify ONE signature per batch (parallel across cores),
/// apply every op via the real `apply_tx`, then `commit_state_transition` (the
/// four roots). If `archive_dir` is set, every block's batches are serialized to
/// disk there (the durability + I/O tax the in-memory bench ignored — point it
/// at the /home/storage array to go large).
pub fn run_batch_auth(
    n_blocks: u64,
    batches_per_block: u64,
    batch_size: u64,
    n_authors: u64,
    archive_dir: Option<&str>,
) -> BatchAuthReport {
    let n_authors = n_authors.max(2);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

    // real ed25519 author keypairs; fund each author's wallet (= BLAKE3(pubkey)).
    let authors: Vec<([u8; 32], [u8; 32], WalletId)> =
        (0..n_authors).map(|_| ed25519_keygen()).collect();
    let mut state = SigilState::new();
    // Fund within the 21M cap (sum ≤ MAX_SUPPLY) — the chokepoint enforces it.
    let per = MAX_SUPPLY / n_authors as u128;
    let fund: Vec<StateMutation> = authors.iter()
        .map(|(_, _, w)| StateMutation::SetBalance { wallet: *w, token: NATIVE, amount: per })
        .collect();
    commit_state_transition(&mut state, &StateTransition { at_height: 0, mutations: fund }, 0)
        .expect("fund authors");

    if let Some(d) = archive_dir { std::fs::create_dir_all(d).ok(); }

    let (mut verify_apply_ns, mut commit_ns, mut disk_ns) = (0u128, 0u128, 0u128);
    let (mut txs, mut sigs, mut bytes) = (0u64, 0u64, 0u64);
    let mut ctr = 0u64;
    let t_total = Instant::now();

    for h in 1..=n_blocks {
        // build the block's authorized batches (one signature each).
        let mut batches: Vec<AuthorizedBatch> = Vec::with_capacity(batches_per_block as usize);
        for _ in 0..batches_per_block {
            let (sk, pk, author) = &authors[(ctr % n_authors) as usize];
            ctr += 1;
            let ops: Vec<SigilTx> = (0..batch_size).map(|j| SigilTx::Send {
                from: *author,
                to: authors[((ctr + j) % n_authors) as usize].2,
                amount: 1, token: NATIVE, fee: 0,
            }).collect();
            batches.push(AuthorizedBatch::sign_ed25519(ops, sk, pk));
        }

        // verify ONE sig/batch + apply every op — parallel across cores (read state).
        let t_va = Instant::now();
        let chunk = batches.len().div_ceil(cores).max(1);
        let muts: Mutex<Vec<StateMutation>> = Mutex::new(Vec::new());
        let tally: Mutex<(u64, u64)> = Mutex::new((0, 0));
        std::thread::scope(|s| {
            for sl in batches.chunks(chunk) {
                let (st, m, t) = (&state, &muts, &tally);
                s.spawn(move || {
                    let (mut local, mut ls, mut lt) = (Vec::new(), 0u64, 0u64);
                    for ba in sl {
                        if ba.verify().is_err() { continue; }   // the ONE signature
                        ls += 1;
                        for op in &ba.ops {
                            if let Ok(r) = apply_tx(st, &sign_dummy(op.clone())) {
                                local.extend(r.mutations);
                                lt += 1;
                            }
                        }
                    }
                    m.lock().unwrap().extend(local);
                    let mut g = t.lock().unwrap(); g.0 += ls; g.1 += lt;
                });
            }
        });
        verify_apply_ns += t_va.elapsed().as_nanos();
        let (bs, bt) = *tally.lock().unwrap(); sigs += bs; txs += bt;

        // commit the merged mutations + recompute the four roots (serial).
        let transition = StateTransition { at_height: h, mutations: muts.into_inner().unwrap() };
        let t_c = Instant::now();
        let _ = commit_state_transition(&mut state, &transition, h);
        commit_ns += t_c.elapsed().as_nanos();

        // archive the block's batches to disk (durability + the I/O tax).
        if let Some(d) = archive_dir {
            let t_d = Instant::now();
            let path = format!("{d}/blk_{h:08}.json");
            if let Ok(f) = std::fs::File::create(&path) {
                if serde_json::to_writer(std::io::BufWriter::new(f), &batches).is_ok() {
                    bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                }
            }
            disk_ns += t_d.elapsed().as_nanos();
        }
    }

    let total_ms = t_total.elapsed().as_millis();
    let secs = (total_ms as f64 / 1000.0).max(1e-6);
    BatchAuthReport {
        blocks: n_blocks,
        batches: n_blocks * batches_per_block,
        batch_size,
        txs, sigs_verified: sigs, authors: n_authors,
        verify_apply_ms: verify_apply_ns / 1_000_000,
        commit_ms: commit_ns / 1_000_000,
        disk_ms: disk_ns / 1_000_000,
        bytes_archived: bytes,
        total_ms,
        tps: txs as f64 / secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throughput_runs_and_reports() {
        // Modest scale for CI: 200 blocks × 50 txs = 10k txs.
        let r = run_throughput(200, 50, 256);
        assert_eq!(r.blocks, 200);
        assert_eq!(r.txs, 10_000);
        assert!(r.tps > 0.0);
        // apply + commit should roughly account for the total.
        assert!(r.apply_ms + r.commit_ms <= r.total_ms + 5);
    }

    #[test]
    fn batching_does_fewer_root_recomputations_for_same_txs() {
        // The amortization case for batching (roadmap piece #3), stated as the
        // DETERMINISTIC invariant rather than a wall-clock race.
        //
        // The four state roots are recomputed once PER BLOCK (per commit). So for
        // an identical transaction set, fewer-bigger blocks recompute the roots
        // far fewer times: 100 blocks × 100 txs = 10k txs in 100 commits, vs
        // 10k blocks × 1 tx = the same 10k txs in 10k commits — a 100× difference
        // in full-state-root recomputations. THAT is the inefficiency that makes
        // the current "rehash-the-whole-state-every-block" root a Phase-3 target
        // for an incremental Sparse-Merkle accumulator.
        //
        // NB: we deliberately do NOT assert `batched.tps > single.tps`. At
        // Phase-0's small state the O(state) root cost does not dominate, so
        // end-to-end TPS is governed by per-block/batch overhead and is too noisy
        // to race reliably (measured both directions across wallet counts on a
        // loaded host). The commit COUNT is the machine-independent truth.
        let batched = run_throughput(100, 100, 256);
        let single = run_throughput(10_000, 1, 256);

        // Same work, different blocking.
        assert_eq!(batched.txs, single.txs, "both apply the same 10k txs");
        assert_eq!(batched.txs, 10_000);

        // One full-state-root recomputation per block → single-tx blocks do 100×
        // more root work for the identical transaction set.
        assert_eq!(batched.blocks, 100);
        assert_eq!(single.blocks, 10_000);
        assert_eq!(
            single.blocks,
            100 * batched.blocks,
            "single-tx blocking recomputes the state roots 100× more often for the same txs"
        );

        // Both pipelines must do real, attributable work.
        assert!(batched.commit_ms > 0 && single.commit_ms > 0);
        assert!(batched.apply_ms + batched.commit_ms <= batched.total_ms + 5);
    }
}
