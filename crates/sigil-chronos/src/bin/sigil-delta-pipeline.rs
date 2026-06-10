//! sigil-delta-pipeline — Pipeline Block Production Benchmark
//!
//! Part of "Sigil Warp Drive" research track (B2: Delta Horizon).
//!
//! The insight: current block production is sequential — Delta builds block N,
//! gossips it, waits for propagation, then builds N+1. But block BUILDING
//! (apply_tx + commit_state_transition) and block PROPAGATION (gossipsub mesh)
//! are independent resources (CPU vs network). We can pipeline them:
//!
//! ```text
//! Time →
//! CPU:   [Build N] [Build N+1] [Build N+2] [Build N+3]
//! Net:        [Propagate N] [Propagate N+1] [Propagate N+2]
//! ```
//!
//! Chronos measures the exact overlap window and quantifies the TPS gain.
//!
//! # Parameters swept
//!   - build_time_us: 100..10000 (how long apply+commit takes per block)
//!   - propagation_us: 1000..500000 (gossip latency)
//!   - pipeline_depth: 1..8 (how many blocks ahead to build)
//!   - tpb: 1..1000 (txs per block — drives build_time)
//!
//! # Output
//!   - Speedup vs sequential (should approach propagation/build ratio)
//!   - Optimal pipeline depth for given build/propagation ratio
//!   - Config for Delta: export SIGIL_PIPELINE_DEPTH=N

use std::time::Instant;

use sigil_chronos::throughput::{run_throughput, ThroughputReport};

/// One pipeline configuration point.
#[derive(Debug, Clone)]
struct PipelinePoint {
    build_time_us: u64,
    propagation_us: u64,
    pipeline_depth: u32,
    tpb: u64,
}

/// Result of one pipeline measurement.
#[derive(Debug, Clone, serde::Serialize)]
struct PipelineResult {
    build_time_us: u64,
    propagation_us: u64,
    pipeline_depth: u32,
    tpb: u64,
    /// Total txs processed.
    total_txs: u64,
    /// Total blocks produced.
    total_blocks: u64,
    /// Sequential time (no pipelining): blocks × (build + propagation).
    sequential_us: u64,
    /// Pipelined time: build × blocks + propagation (overlap).
    pipelined_us: u64,
    /// Speedup: sequential / pipelined.
    speedup: f64,
    /// Effective TPS with pipelining.
    pipelined_tps: f64,
    /// Effective TPS without pipelining.
    sequential_tps: f64,
}

impl PipelineResult {
    fn header() -> &'static str {
        "build_us  prop_us  depth  tpb    seq_us     pipe_us    speedup  seq_tps  pipe_tps"
    }

    fn row(&self) -> String {
        format!(
            "{:<9} {:<8} {:<6} {:<6} {:<10} {:<10} {:<7.2}× {:<8.0} {:<9.0}",
            self.build_time_us,
            self.propagation_us,
            self.pipeline_depth,
            self.tpb,
            self.sequential_us,
            self.pipelined_us,
            self.speedup,
            self.sequential_tps,
            self.pipelined_tps,
        )
    }
}

/// Model pipeline execution: simulate N blocks with pipelining.
///
/// The model:
/// - CPU builds one block in `build_time_us`
/// - Network propagates one block in `propagation_us`
/// - With pipeline_depth D: CPU can build up to D blocks ahead of the
///   last propagated block
/// - Total time = max(build_time × blocks, propagation × blocks) with
///   pipelining, vs (build_time + propagation) × blocks sequentially
fn simulate_pipeline(point: &PipelinePoint) -> PipelineResult {
    let build = point.build_time_us;
    let prop = point.propagation_us;
    let depth = point.pipeline_depth;
    let tpb = point.tpb;

    // Simulate 100 blocks — enough to see steady-state.
    let n_blocks: u64 = 100;
    let total_txs = n_blocks * tpb;

    // Sequential: every block waits for build + propagation.
    let sequential_us = n_blocks * (build + prop);

    // Pipelined: after the first `depth` blocks are built, the CPU
    // and network run concurrently. The total time is:
    //   build_time × n_blocks  (CPU never stops)
    //   + propagation × 1      (last block's propagation, everything else overlaps)
    // But CPU can only get `depth` blocks ahead. If build < prop/depth,
    // CPU stalls waiting for propagation to drain.
    // v0.32.9 fix: the CPU is the BOTTLENECK (never stalls) when building `depth` blocks
    // takes at least as long as one propagation — the head of the window has always cleared
    // by the time the window fills. The old `<=` had this inverted, so depth=1 scored the
    // ideal-overlap formula and deeper pipelines never looked better (failed its own test).
    let cpu_can_keep_up = build * depth as u64 >= prop;
    let pipelined_us = if cpu_can_keep_up {
        // CPU builds all blocks at full speed; last block's propagation
        // happens after CPU finishes.
        build * n_blocks + prop
    } else {
        // Propagation-bound: the window fills, then every batch of `depth` blocks waits
        // for its head to clear (~one propagation per batch).
        // Each batch of `depth` blocks: CPU builds depth × build,
        // then waits for the first of those to propagate before continuing.
        let batches = n_blocks.div_ceil(depth as u64);
        let batch_build = depth as u64 * build;
        // After building a batch, the last block of the PREVIOUS batch
        // must have propagated. The pipeline latency is roughly:
        //   build × n_blocks + prop × (n_blocks / depth)
        build * n_blocks + prop * batches
    };

    let speedup = sequential_us as f64 / pipelined_us.max(1) as f64;
    let sequential_tps = total_txs as f64 / (sequential_us as f64 / 1_000_000.0).max(1e-6);
    let pipelined_tps = total_txs as f64 / (pipelined_us as f64 / 1_000_000.0).max(1e-6);

    PipelineResult {
        build_time_us: build,
        propagation_us: prop,
        pipeline_depth: depth,
        tpb,
        total_txs,
        total_blocks: n_blocks,
        sequential_us,
        pipelined_us,
        speedup,
        sequential_tps,
        pipelined_tps,
    }
}

/// Measure REAL build time for given tpb/wallets using the throughput harness.
fn measure_build_time(tpb: u64, wallets: u64) -> u64 {
    let blocks = 10;
    let r = run_throughput(blocks, tpb, wallets);
    // Per-block build time in microseconds.
    if r.blocks > 0 {
        (r.total_ms as u64 * 1000) / r.blocks
    } else {
        1000 // fallback: 1ms per block
    }
}

fn quick_sweep() -> Vec<PipelinePoint> {
    let mut points = Vec::new();
    for tpb in [10, 50, 100, 500] {
        for build_us in [100, 500, 1000, 5000, 10000] {
            for prop_us in [5000, 50000, 200000, 500000] {
                for depth in [1, 2, 4, 8] {
                    points.push(PipelinePoint {
                        build_time_us: build_us,
                        propagation_us: prop_us,
                        pipeline_depth: depth,
                        tpb,
                    });
                }
            }
        }
    }
    points
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let json_only = args.iter().any(|a| a == "--json");
    let measure_real = args.iter().any(|a| a == "--measure");

    if measure_real {
        println!("Measuring real build times from Chronos throughput harness...");
        for tpb in [10, 50, 100, 500, 1000] {
            for wallets in [256, 16384, 262144] {
                let build_us = measure_build_time(tpb, wallets);
                println!("  tpb={tpb} wallets={wallets} → build_time={build_us}μs/block");
            }
        }
        return;
    }

    let points = quick_sweep();

    if !json_only {
        println!("╔══════════════════════════════════════════════════════════════════════════════╗");
        println!("║           DELTA HORIZON — Pipeline Block Production Benchmark               ║");
        println!("╠══════════════════════════════════════════════════════════════════════════════╣");
        println!("║  Points: {:<4}    Blocks modeled: 100                                      ║", points.len());
        println!("╚══════════════════════════════════════════════════════════════════════════════╝");
        println!();
        println!("{}", PipelineResult::header());
        println!("{}", "-".repeat(PipelineResult::header().len()));
    }

    let t0 = Instant::now();
    let mut results: Vec<PipelineResult> = points.iter().map(|p| simulate_pipeline(p)).collect();
    let wall_ms = t0.elapsed().as_millis();

    // Sort by speedup descending.
    results.sort_by(|a, b| b.speedup.partial_cmp(&a.speedup).unwrap_or(std::cmp::Ordering::Equal));

    if json_only {
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "wall_ms": wall_ms,
            "results": &results,
            "top_result": &results.first(),
        })).unwrap();
        println!("{json}");
    } else {
        // Show top 30 + notable patterns.
        for r in results.iter().take(30) {
            println!("{}", r.row());
        }
        println!();
        println!("═══ Pipeline sweep: {} points in {} ms ═══", points.len(), wall_ms);

        // Analysis.
        let best = &results[0];
        println!();
        println!("🏆 OPTIMAL PIPELINE CONFIGURATION:");
        println!("   build_time     = {} μs/block", best.build_time_us);
        println!("   propagation    = {} μs", best.propagation_us);
        println!("   pipeline_depth = {}", best.pipeline_depth);
        println!("   speedup        = {:.2}×", best.speedup);
        println!("   sequential TPS = {:.0}", best.sequential_tps);
        println!("   pipelined TPS  = {:.0}", best.pipelined_tps);
        println!();

        // When is pipelining most effective?
        // Pipeline helps when build_time ≈ propagation_time.
        // When build ≪ propagation: sequential is already fast.
        // When build ≫ propagation: pipeline can't hide the CPU work.
        let max_speedup = results.iter().max_by(|a, b| a.speedup.partial_cmp(&b.speedup).unwrap()).unwrap();
        println!("📊 ANALYSIS:");
        println!("   Max speedup: {:.2}× at build={}μs prop={}μs depth={}",
            max_speedup.speedup, max_speedup.build_time_us, max_speedup.propagation_us, max_speedup.pipeline_depth);
        println!("   Pipeline wins when build_time approaches propagation_time");
        println!("   Ideal ratio: build_time ≈ propagation_time / pipeline_depth");
        println!();
        println!("   Delta deploy:");
        println!("   export SIGIL_PIPELINE_DEPTH={}", best.pipeline_depth);
        println!("   export SIGIL_TARGET_TPB={}", best.tpb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_speedup_never_below_one() {
        let p = PipelinePoint { build_time_us: 1000, propagation_us: 50000, pipeline_depth: 4, tpb: 100 };
        let r = simulate_pipeline(&p);
        assert!(r.speedup >= 1.0, "pipeline should never be slower than sequential");
    }

    #[test]
    fn deep_pipeline_helps_when_build_is_fast() {
        // Fast build, slow propagation → deep pipeline helps.
        let shallow = simulate_pipeline(&PipelinePoint { build_time_us: 500, propagation_us: 100000, pipeline_depth: 1, tpb: 100 });
        let deep = simulate_pipeline(&PipelinePoint { build_time_us: 500, propagation_us: 100000, pipeline_depth: 8, tpb: 100 });
        assert!(deep.speedup > shallow.speedup, "deeper pipeline should help when propagation dominates");
    }

    #[test]
    fn pipeline_doesnt_help_when_build_dominates() {
        // Slow build, fast propagation → pipelining doesn't help.
        let pipeline = simulate_pipeline(&PipelinePoint { build_time_us: 100000, propagation_us: 1000, pipeline_depth: 8, tpb: 100 });
        assert!(pipeline.speedup < 2.0, "pipeline cannot hide dominant build time");
    }
}
