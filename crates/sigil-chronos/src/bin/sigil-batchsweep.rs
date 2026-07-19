//! Batch-auth TPS sweep — how batching converts the state-commit ceiling
//! (13M sound / 209M unsafe commits/s) into usable TPS.
//!
//! Each run drives the REAL pipeline via `run_batch_auth`: verify ONE ed25519
//! signature per `AuthorizedBatch` (parallel across cores), apply every op via
//! the real `apply_tx`, then `commit_state_transition` (the four state roots).
//! Sweeping `batch_size` = ops-per-signature moves the bottleneck from signature
//! verification (batch_size = 1, sig-bound) toward pure state-fold (large batch,
//! commit-bound) — so TPS climbs toward the state ceiling as sig cost amortizes.
//!
//! Usage: sigil-batchsweep [total_ops] [authors] [archive_dir]
//!   total_ops   ~ops applied per run (default 4_000_000)
//!   authors     distinct signers        (default 1024)
//!   archive_dir if set, also pays the disk-archive tax (else pure in-mem ceiling)

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let total_ops: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(4_000_000);
    let authors: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(1024);
    let archive: Option<&str> = a.get(3).map(|s| s.as_str());

    println!("🔬 BATCH-AUTH TPS SWEEP — ~{total_ops} ops/run · {authors} authors · {} \n",
        archive.map(|d| format!("archiving → {d}")).unwrap_or_else(|| "in-memory (pure pipeline ceiling)".into()));
    println!("  {:>9} │ {:>13} │ {:>10} │ {:>22} │ {:>18}",
        "ops/sig", "TPS", "sigs", "verify+apply / commit(ms)", "sig share of time");

    for batch_size in [1u64, 8, 64, 512, 4096, 32_768] {
        // Hold total ops ~constant; spread batches over up to 100 blocks.
        let batches = (total_ops / batch_size).max(1);
        let n_blocks = batches.min(100).max(1);
        let batches_per_block = (batches / n_blocks).max(1);
        let r = sigil_chronos::throughput::run_batch_auth(n_blocks, batches_per_block, batch_size, authors, archive);
        let ops_per_sig = r.txs as f64 / r.sigs_verified.max(1) as f64;
        // verify+apply lumps sig-verify and state-apply; as batch grows the sig part
        // shrinks, so this column's TREND is the amortization signal.
        let sig_share = 100.0 * r.verify_apply_ms as f64 / (r.verify_apply_ms + r.commit_ms).max(1) as f64;
        println!("  {:>9.0} │ {:>9.0} TPS │ {:>10} │ {:>10} / {:<9} │ {:>16.0}%",
            ops_per_sig, r.tps, r.sigs_verified, r.verify_apply_ms, r.commit_ms, sig_share);
    }
    println!("\n  ceiling reference: 13M sound / 209M unsafe state commits/s (Stargate handoff).");
    println!("  the gap between the large-batch TPS and that ceiling = the real remaining bottleneck.");
}
