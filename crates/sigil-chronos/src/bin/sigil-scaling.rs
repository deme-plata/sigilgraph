//! Horizontal-scaling + node-density sweep.
//!
//! ```text
//! sigil-scaling                 # full sweep: archive-shard, replica, light
//! sigil-scaling light  10000    # light nodes only, 10k verifies each
//! ```
//!
//! Measures, on real OS threads over real cores, whether SIGIL throughput
//! scales horizontally and how many light (10 ms tip-verify, no archive) nodes
//! a single host sustains.

use sigil_chronos::scaling::{sweep, ScalingPoint};

fn print_table(title: &str, pts: &[ScalingPoint]) {
    println!("\n  {title}");
    println!("  {}", "─".repeat(96));
    for p in pts {
        println!("  {}", p.line());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("🛰  SIGIL horizontal-scaling sweep · host has {cores} cores\n");

    // Node-count ladder: 1 → past core count to see the plateau.
    let counts: Vec<u64> = vec![1, 2, 4, 8, 16, 24, 32, 48, 64, 96, 128]
        .into_iter()
        .filter(|&n| n <= 256)
        .collect();

    if args.len() >= 2 && args[1] == "light" {
        let verifies: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);
        let pts = sweep("light", &counts, 0, 0, 0, verifies);
        print_table(
            &format!("LIGHT nodes (10 ms tip-verify only, O(1) RAM) · {verifies} verifies/node"),
            &pts,
        );
        return;
    }

    // `sigil-scaling nodes <N>` — run exactly N parallel nodes of each class.
    // This is the "1000 nodes on one host" test: N light tip-verify nodes (the
    // cheap class you pack thousands of) + N archive shards (the heavy class).
    if args.len() >= 2 && args[1] == "nodes" {
        let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
        println!("  Running {n} parallel nodes of each class on {cores} cores\n");
        let light = sweep("light", &[1, n], 0, 0, 0, 50_000);
        print_table(&format!("LIGHT — {n} tip-verify nodes (O(1) RAM, no history)"), &light);
        // Archive (full chains) is RAM-heavy; nobody runs 1000 full chains on
        // one host, so cap the archive class at the core count — the realistic
        // "few heavy + many light" topology.
        let arch_n = n.min(cores as u64);
        let archive = sweep("archive-shard", &[1, arch_n], 50, 20, 128, 0);
        print_table(&format!("ARCHIVE — {arch_n} full-chain shards (capped at cores)"), &archive);
        return;
    }

    // ARCHIVE shards — independent full chains, the real horizontal scaling.
    // 200 blocks × 50 txs = 10k txs per shard — enough to dwarf thread setup.
    let archive = sweep("archive-shard", &counts, 200, 50, 256, 0);
    print_table("ARCHIVE shards (full apply+commit+4 roots, independent chains)", &archive);

    // REPLICA — N validators of ONE chain. Redundancy, not speed.
    let replica = sweep("replica", &[1, 2, 4, 8, 16, 32], 200, 50, 256, 0);
    print_table("REPLICA (N validators, same chain) — useful TPS falls ~1/N", &replica);

    // LIGHT — tip-verify-only, the node class you pack thousands of.
    let light = sweep("light", &counts, 0, 0, 0, 200_000);
    print_table("LIGHT nodes (10 ms tip-verify only, O(1) RAM, no history)", &light);

    println!("\n  Read the three tables together:");
    println!("  • archive-shard speedup climbing toward N = horizontal scaling works (independent lanes)");
    println!("  • replica agg_ops ~flat/falling = adding validators ≠ more throughput");
    println!("  • light per-node ops huge + flat = thousands of light nodes are cheap (CPU-bound, not RAM)");
}
