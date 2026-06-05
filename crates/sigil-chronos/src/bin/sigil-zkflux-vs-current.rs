//! Does the new zk-flux (post-quantum ZK hybrid) beat our current ed25519
//! batch-auth verification path? Runs both through the chronos comparison
//! harness and writes the table to a file (so the numbers are read-from-file,
//! never typed from memory).
//!
//! ```text
//! sigil-zkflux-vs-current                 # default sweep, writes /tmp/zkflux_vs_current.txt
//! sigil-zkflux-vs-current <out_file>      # custom output path
//! ```

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let out = std::env::args().nth(1).unwrap_or_else(|| "/tmp/zkflux_vs_current.txt".to_string());

    // Block tx-counts (= zk-flux trace rows). Capped at what actually proves in
    // bounded time on this box; nothing extrapolated past what ran.
    let txs_sweep = [1024u64, 4096, 16384];
    // Two regimes: ops/batch = 1 (one sig per tx, no batching) and 256
    // (production batch-auth — one sig authorizes 256 ops).
    let ops_sweep = [1u64, 256];

    eprintln!("running zk-flux vs current comparison (txs {txs_sweep:?} × ops/batch {ops_sweep:?})…");
    let points = sigil_chronos::zkflux::run_comparison(&txs_sweep, &ops_sweep).await?;
    let report = sigil_chronos::zkflux::render(&points);

    std::fs::write(&out, &report)?;
    print!("{report}");
    eprintln!("\n→ written to {out}");
    Ok(())
}
