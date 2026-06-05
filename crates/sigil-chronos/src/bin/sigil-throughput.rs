//! Play the SIGIL chain at scale + measure the throughput ceiling.
//!
//! ```text
//! sigil-throughput [blocks] [txs_per_block] [wallets]
//! sigil-throughput            # default sweep
//! sigil-throughput 10000 100  # 10k blocks × 100 txs = 1M txs
//! ```

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() >= 3 {
        let blocks: u64 = a[1].parse().unwrap_or(1000);
        let tpb: u64 = a[2].parse().unwrap_or(100);
        let wallets: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(1024);
        let r = sigil_chronos::throughput::run_throughput(blocks, tpb, wallets);
        println!("🚀 {}", r.summary());
        return;
    }

    // Default sweep — shows the ceiling + the apply-vs-roots split scaling.
    println!("🚀 SIGIL throughput sweep (real apply_tx + commit pipeline)\n");
    for (blocks, tpb, wallets) in [
        (1_000u64, 100u64, 1024u64),    // 100k txs
        (1_000, 1_000, 1024),           // 1M txs, big blocks
        (10_000, 100, 1024),            // 1M txs, the 10k-blocks target shape
    ] {
        let r = sigil_chronos::throughput::run_throughput(blocks, tpb, wallets);
        println!("• {}\n", r.summary());
    }
}
