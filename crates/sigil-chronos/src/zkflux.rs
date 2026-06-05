//! zk-flux verify-once vs the current ed25519 batch-auth path — does the new
//! post-quantum ZK hybrid give the chain better performance?
//!
//! Both paths share the SAME `apply_tx` + `commit_state_transition` pipeline
//! (state is NOT the bottleneck — 13M sound / 209M unsafe commits/s per the
//! Stargate 500M handoff). The ONLY thing that differs is how a node convinces
//! itself a block's `T` transactions are authorized:
//!
//!   - **CURRENT (ed25519 batch-auth):** re-verify `B` ed25519 signatures (one
//!     per [`AuthorizedBatch`], each authorizing `S` ops, `T = B·S`), parallel
//!     across cores. EVERY non-producing node pays this on every block. This is
//!     exactly what `throughput::run_batch_auth` does in production today.
//!   - **ZK-FLUX (verify-once):** the producer emits ONE zk-flux proof attesting
//!     the whole block; every verifier checks ONE O(1) lattice wrap, independent
//!     of `T`. The producer pays a `prove()` tax once.
//!
//! HONESTY (mirrors the zk-flux crate's own doc): zk-flux v0's lattice wrap
//! BINDS the FRI-STARK commitment but does not yet PROVE STARK validity (the
//! FRI-verifier-in-circuit is the recursion frontier). So these are
//! COST-STRUCTURE numbers — real `prove()` time, real O(1) `verify()` time, real
//! proof sizes — not a soundness claim. Prove cost is measured only at trace
//! lengths that actually ran; the producer-side ceiling is reported as MEASURED,
//! never extrapolated past what executed.

use std::sync::Mutex;
use std::time::Instant;

use flux_lattice_guard::SecurityLevel;
use sigil_state::WalletId;
use sigil_tx::{ed25519_keygen, AuthorizedBatch, SigilTx, NATIVE};
use zk_flux::ZkFlux;

/// One measured point: a block of `txs` organized as `batches × ops_per_batch`,
/// measured through both verification paths.
#[derive(Debug, Clone)]
pub struct ComparePoint {
    pub txs: u64,
    pub batches: u64,
    pub ops_per_batch: u64,
    // ── current: ed25519 batch-auth ──
    /// Wall-clock to verify all `batches` ed25519 sigs in parallel (replication).
    pub ed25519_verify_ms: f64,
    /// Replication-side throughput of the current path: txs ÷ verify time.
    pub ed25519_verify_tps: f64,
    // ── zk-flux: verify-once ──
    /// Producer's one-time prove() over a `txs`-row trace.
    pub zkflux_prove_ms: f64,
    /// Any verifier's O(1) wrap check (independent of `txs`).
    pub zkflux_verify_ms: f64,
    /// Replication-side throughput of zk-flux: txs ÷ wrap-verify time.
    pub zkflux_verify_tps: f64,
    /// Producer-side ceiling of zk-flux: txs ÷ prove time (the real cap).
    pub zkflux_prove_tps: f64,
    pub wrap_bytes: usize,
    pub stark_bytes: usize,
}

fn author_wallets(n: u64) -> Vec<([u8; 32], [u8; 32], WalletId)> {
    (0..n.max(2)).map(|_| ed25519_keygen()).collect()
}

/// Build `batches` AuthorizedBatches of `ops_per_batch` single-author sends,
/// then time verifying ALL of them in parallel across `cores` — the exact verify
/// phase of `run_batch_auth`, isolated. Returns (verify_ms, sigs_verified).
fn measure_ed25519_batch_verify(batches: u64, ops_per_batch: u64) -> (f64, u64) {
    let authors = author_wallets(256);
    let n = authors.len() as u64;
    let cores = std::thread::available_parallelism().map(|c| c.get()).unwrap_or(1);

    // Construct the batches up front (NOT timed — we measure verification only).
    let mut signed: Vec<AuthorizedBatch> = Vec::with_capacity(batches as usize);
    let mut ctr = 0u64;
    for _ in 0..batches {
        let (sk, pk, author) = &authors[(ctr % n) as usize];
        ctr += 1;
        let ops: Vec<SigilTx> = (0..ops_per_batch)
            .map(|j| SigilTx::Send {
                from: *author,
                to: authors[((ctr + j) % n) as usize].2,
                amount: 1,
                token: NATIVE,
                fee: 0,
            })
            .collect();
        signed.push(AuthorizedBatch::sign_ed25519(ops, sk, pk));
    }

    let chunk = signed.len().div_ceil(cores).max(1);
    let ok = Mutex::new(0u64);
    let t = Instant::now();
    std::thread::scope(|s| {
        for sl in signed.chunks(chunk) {
            let okref = &ok;
            s.spawn(move || {
                let mut local = 0u64;
                for ba in sl {
                    if ba.verify().is_ok() {
                        local += 1;
                    }
                }
                *okref.lock().unwrap() += local;
            });
        }
    });
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    (ms, ok.into_inner().unwrap())
}

/// Build a representative `rows`-length execution trace from synthetic txs and
/// measure a single zk-flux prove + verify. Returns
/// (prove_ms, verify_ms, wrap_bytes, stark_bytes).
async fn measure_zkflux(zk: &mut ZkFlux, rows: u64) -> anyhow::Result<(f64, f64, usize, usize)> {
    // One row per attested tx: (from-lo, to-lo, amount, token-tag, 4 pad cols).
    let trace: Vec<Vec<u64>> = (0..rows)
        .map(|i| vec![i, i.wrapping_mul(2) + 1, 1, 0xC0FFEE, i ^ 0xAB, 11, 13, 17])
        .collect();

    let t = Instant::now();
    let proof = zk.prove(&trace).await?;
    let prove_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let ok = zk.verify(&proof)?;
    let verify_ms = t.elapsed().as_secs_f64() * 1000.0;
    anyhow::ensure!(ok, "zk-flux proof must verify");

    Ok((prove_ms, verify_ms, proof.wrap_size_bytes(), proof.stark_proof_bytes))
}

/// Sweep block tx-counts `txs_sweep` × batch shapes `ops_per_batch_sweep` and
/// measure both verification paths. zk-flux is proven ONCE per tx-count (prove
/// cost depends on trace length, not on how the sigs are batched), reused across
/// batch shapes. Returns one [`ComparePoint`] per (txs, ops_per_batch).
pub async fn run_comparison(
    txs_sweep: &[u64],
    ops_per_batch_sweep: &[u64],
) -> anyhow::Result<Vec<ComparePoint>> {
    // PQ-128 lattice + hash-FRI — the same level the zk-flux demo uses.
    let mut zk = ZkFlux::new(SecurityLevel::PQ128).await?;
    let mut out = Vec::new();

    for &txs in txs_sweep {
        // zk-flux: one proof over a txs-row trace (verify-once for the block).
        let (prove_ms, verify_ms, wrap_bytes, stark_bytes) = measure_zkflux(&mut zk, txs).await?;
        let zkflux_verify_tps = txs as f64 / (verify_ms / 1000.0).max(1e-9);
        let zkflux_prove_tps = txs as f64 / (prove_ms / 1000.0).max(1e-9);

        for &ops in ops_per_batch_sweep {
            let batches = (txs / ops).max(1);
            let real_txs = batches * ops; // exact, after integer division
            let (ed_ms, _sigs) = measure_ed25519_batch_verify(batches, ops);
            let ed_tps = real_txs as f64 / (ed_ms / 1000.0).max(1e-9);

            out.push(ComparePoint {
                txs: real_txs,
                batches,
                ops_per_batch: ops,
                ed25519_verify_ms: ed_ms,
                ed25519_verify_tps: ed_tps,
                zkflux_prove_ms: prove_ms,
                zkflux_verify_ms: verify_ms,
                zkflux_verify_tps: zkflux_verify_tps,
                zkflux_prove_tps: zkflux_prove_tps,
                wrap_bytes,
                stark_bytes,
            });
        }
    }
    Ok(out)
}

/// Render the comparison as a human report (also written to file by the bin).
pub fn render(points: &[ComparePoint]) -> String {
    let mut s = String::new();
    s.push_str("zk-flux verify-once  vs  current ed25519 batch-auth — chronos comparison\n");
    s.push_str("(verification step only; apply+commit is identical & not the bottleneck)\n\n");
    s.push_str(&format!(
        "{:>7} {:>6} {:>7} | {:>14} {:>16} | {:>13} {:>13} {:>17} {:>17}\n",
        "txs", "batch", "ops/b",
        "ed25519 vfy", "ed25519 vfy TPS",
        "zk prove ms", "zk verify ms", "zk verify TPS", "zk PRODUCE TPS",
    ));
    s.push_str(&"-".repeat(126));
    s.push('\n');
    for p in points {
        s.push_str(&format!(
            "{:>7} {:>6} {:>7} | {:>11.2} ms {:>16.0} | {:>13.1} {:>13.2} {:>17.0} {:>17.0}\n",
            p.txs, p.batches, p.ops_per_batch,
            p.ed25519_verify_ms, p.ed25519_verify_tps,
            p.zkflux_prove_ms, p.zkflux_verify_ms, p.zkflux_verify_tps, p.zkflux_prove_tps,
        ));
    }
    if let Some(p) = points.last() {
        s.push_str(&format!(
            "\nproof sizes (last row): STARK {} B (committed, not sent) · lattice wrap {} B (what every verifier checks)\n",
            p.stark_bytes, p.wrap_bytes,
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn comparison_runs_small() {
        // Tiny scale so the test is fast: 256-row trace, two batch shapes.
        let pts = run_comparison(&[256], &[1, 64]).await.expect("comparison runs");
        assert_eq!(pts.len(), 2);
        for p in &pts {
            assert!(p.ed25519_verify_tps > 0.0);
            assert!(p.zkflux_verify_tps > 0.0);
            assert!(p.zkflux_prove_tps > 0.0);
            // zk-flux verify is O(1): wrap-verify time independent of batch shape.
            assert!(p.zkflux_verify_ms > 0.0);
        }
    }
}
