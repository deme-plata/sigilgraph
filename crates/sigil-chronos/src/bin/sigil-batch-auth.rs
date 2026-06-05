//! Batch-authorization through the REAL SIGIL pipeline — does one-sig-per-N-ops
//! make the state-commit ceiling usable as TPS when apply_tx + commit + the four
//! roots actually run (and optionally when every block is archived to disk)?
//!
//! ```text
//! sigil-batch-auth                                   # batch-size sweep (in-memory)
//! sigil-batch-auth <blocks> <batches/blk> <ops/batch> <authors> [archive_dir]
//! # large disk-backed run on the 80T array:
//! sigil-batch-auth 20000 64 256 100000 /home/storage/sigil-bench/batch-archive
//! ```

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() >= 5 {
        let blocks: u64 = a[1].parse().unwrap_or(1000);
        let bpb: u64 = a[2].parse().unwrap_or(64);
        let bs: u64 = a[3].parse().unwrap_or(256);
        let authors: u64 = a[4].parse().unwrap_or(10000);
        let archive = a.get(5).map(|s| s.as_str());
        if let Some(d) = archive { println!("🗄  archiving every block → {d}  (using the /home/storage array)"); }
        let r = sigil_chronos::throughput::run_batch_auth(blocks, bpb, bs, authors, archive);
        println!("🔐 {}", r.summary());
        return;
    }

    // Default: walk batch size through the REAL pipeline (in-memory, no archive).
    // Shows the signature amortizing away + where the real bottleneck lands.
    println!("🔐 BATCH-AUTH through the REAL SIGIL pipeline (apply_tx + commit + 4 roots)\n");
    println!("   one ed25519 signature authorizes B single-author ops; 4096 batches/block, 50 blocks.\n");
    let (blocks, bpb, authors) = (50u64, 4096u64, 100_000u64);
    for bs in [1u64, 16, 64, 256, 1024] {
        let r = sigil_chronos::throughput::run_batch_auth(blocks, bpb, bs, authors, None);
        let ops_per_sig = r.txs as f64 / r.sigs_verified.max(1) as f64;
        println!("  B={bs:>5}  {tps:>13.0} TPS   ({ops_per_sig:.0} ops/sig · {sv} sigs · {txs} txs)",
                 tps = r.tps, sv = r.sigs_verified, txs = r.txs);
    }
    println!("\n  → B=1 is the per-sig wall; as B grows the signature amortizes and the");
    println!("    real apply+commit+roots pipeline becomes the bottleneck (the true TPS).");
}
