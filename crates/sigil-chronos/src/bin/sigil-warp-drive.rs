//! sigil-warp-drive — TPS Parameter Sweep Engine
//!
//! Part of "Sigil Warp Drive" research track. Sweeps the Sigil throughput
//! harness across every parameter combination to find the absolute maximum
//! TPS for given hardware. Outputs JSON that Delta can consume directly.
//!
//! Sweep axes:
//!   - blocks: 100..=10_000 (step 100)
//!   - tpb (txs per block): 1..=1000 (step 10)
//!   - wallets: 256, 1024, 4096, 16384, 65536, 262144, 1048576
//!   - threads: 1, 2, 4, 8, 16, 32, 64, 128
//!
//! Run: sigil-warp-drive [--quick] [--full] [--json]
//!   --quick:   reduced sweep, ~30s (for CI/quick checks)
//!   --full:    full sweep, ~5-10 min (for release)
//!   --json:    output JSON only (for Delta consumption)

use std::time::Instant;

use sigil_chronos::throughput::{run_throughput, ThroughputReport};
use rayon::prelude::*;

/// One parameter point to measure.
#[derive(Debug, Clone)]
struct SweepPoint {
    blocks: u64,
    tpb: u64,
    wallets: u64,
}

/// One measured result.
#[derive(Debug, Clone, serde::Serialize)]
struct SweepResult {
    blocks: u64,
    tpb: u64,
    wallets: u64,
    txs: u64,
    apply_ms: f64,
    commit_ms: f64,
    total_ms: f64,
    effective_tps: f64,
    apply_pct: f64,
    commit_pct: f64,
}

impl SweepResult {
    fn from_report(p: &SweepPoint, r: &ThroughputReport) -> Self {
        let total_s = (r.total_ms as f64 / 1000.0).max(1e-6);
        Self {
            blocks: p.blocks,
            tpb: p.tpb,
            wallets: p.wallets,
            txs: r.txs,
            apply_ms: r.apply_ms as f64,
            commit_ms: r.commit_ms as f64,
            total_ms: r.total_ms as f64,
            effective_tps: r.txs as f64 / total_s,
            apply_pct: if r.total_ms > 0 {
                r.apply_ms as f64 / r.total_ms as f64 * 100.0
            } else {
                0.0
            },
            commit_pct: if r.total_ms > 0 {
                r.commit_ms as f64 / r.total_ms as f64 * 100.0
            } else {
                0.0
            },
        }
    }

    fn header() -> &'static str {
        "blocks  tpb     wallets    txs      apply_ms  commit_ms  total_ms   tps        apply%  commit%"
    }

    fn row(&self) -> String {
        format!(
            "{:<7} {:<7} {:<10} {:<8} {:<9.2} {:<10.2} {:<10.2} {:<10.0} {:<6.1}% {:<6.1}%",
            self.blocks,
            self.tpb,
            self.wallets,
            self.txs,
            self.apply_ms,
            self.commit_ms,
            self.total_ms,
            self.effective_tps,
            self.apply_pct,
            self.commit_pct,
        )
    }
}

fn quick_sweep() -> Vec<SweepPoint> {
    let mut points = Vec::new();
    for blocks in [100, 500, 1000, 5000] {
        for tpb in [1, 10, 50, 100, 500] {
            for wallets in [256, 16384, 262144] {
                points.push(SweepPoint {
                    blocks: blocks as u64,
                    tpb: tpb as u64,
                    wallets: wallets as u64,
                });
            }
        }
    }
    points
}

fn full_sweep() -> Vec<SweepPoint> {
    let mut points = Vec::new();
    // Blocks: logarithmic-ish
    for blocks in [100, 200, 500, 1000, 2000, 5000, 10000] {
        // TPB: linear
        for tpb in [1, 5, 10, 25, 50, 100, 250, 500, 1000] {
            // Wallets: powers of 4
            for wallets in [256, 1024, 4096, 16384, 65536, 262144, 1048576] {
                points.push(SweepPoint {
                    blocks: blocks as u64,
                    tpb: tpb as u64,
                    wallets: wallets as u64,
                });
            }
        }
    }
    points
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let json_only = args.iter().any(|a| a == "--json");
    let full = args.iter().any(|a| a == "--full");

    let points = if full { full_sweep() } else { quick_sweep() };

    if !json_only {
        println!("╔══════════════════════════════════════════════════════════════════════════════════════╗");
        println!("║                    SIGIL WARP DRIVE — TPS Parameter Sweep                          ║");
        println!("╠══════════════════════════════════════════════════════════════════════════════════════╣");
        println!("║  Points: {:<4}  Mode: {:<8}  Threads: {} (auto)                                  ║",
            points.len(),
            if full { "full" } else { "quick" },
            rayon::current_num_threads(),
        );
        println!("╚══════════════════════════════════════════════════════════════════════════════════════╝");
        println!();
        println!("{}", SweepResult::header());
        println!("{}", "-".repeat(SweepResult::header().len()));
    }

    let t0 = Instant::now();

    // Run all points in parallel via Rayon — each point is an independent
    // Chronos throughput run that doesn't share state.
    let results: Vec<SweepResult> = points
        .par_iter()
        .map(|p| {
            let r = run_throughput(p.blocks, p.tpb, p.wallets);
            SweepResult::from_report(p, &r)
        })
        .collect();

    let wall_ms = t0.elapsed().as_millis();

    // Sort by TPS descending so the best is on top.
    let mut sorted = results;
    sorted.sort_by(|a, b| b.effective_tps.partial_cmp(&a.effective_tps).unwrap_or(std::cmp::Ordering::Equal));

    if json_only {
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "wall_ms": wall_ms,
            "points": points.len(),
            "results": &sorted,
            "top_result": &sorted.first(),
        }))
        .unwrap();
        println!("{json}");
    } else {
        for r in &sorted {
            println!("{}", r.row());
        }
        println!();
        println!("═══ Sweep complete: {} points in {} ms ═══", points.len(), wall_ms);

        if let Some(best) = sorted.first() {
            println!();
            println!("🏆 OPTIMAL CONFIGURATION:");
            println!("   blocks  = {}", best.blocks);
            println!("   tpb     = {}", best.tpb);
            println!("   wallets = {}", best.wallets);
            println!("   TPS     = {:.0}", best.effective_tps);
            println!("   apply%  = {:.1}%", best.apply_pct);
            println!("   commit% = {:.1}%", best.commit_pct);
            println!();
            println!("   Delta deploy:");
            println!("   export SIGIL_BLOCK_SIZE={}", best.tpb);
            println!("   export SIGIL_TARGET_WALLETS={}", best.wallets);
        }
    }
}
