//! End-to-end wiring reference for the V7-INTEGRATE lane: ONE shared RateGate spine + an
//! `OnlineTuner` driving the pipeline knobs. Uses the analytic model as a stand-in "plant" so
//! it runs with NO live node — to go live, replace the marked block with real per-stage
//! telemetry (each stage fills a `RawStage`).
//!
//! Run: `cargo run --example online_loop`

use std::sync::Arc;

use sigil_synctune::{
    model_eval, BackpressureSpine, KnobSet, OnlineTuner, RateGate, RawStage, RealClock, Stage,
    TARGET_BLK_S,
};

fn main() {
    let cores = 48;

    // ONE shared backpressure spine. Hand this SAME Arc to v7-supply (serve) and v7-ingest
    // (commit): every stage calls `spine.admit(n)` before processing n blocks, so the producer
    // can never overrun ingest.
    let spine: Arc<dyn RateGate> = Arc::new(BackpressureSpine::new(
        Arc::new(RealClock::default()),
        TARGET_BLK_S,
        4096, // burst blocks
        Stage::COUNT,
    ));
    println!(
        "spine admit(128) = {} @ {} blk/s\n",
        spine.admit(128),
        spine.admit_rate()
    );

    // ONE online tuner per node.
    let mut tuner = OnlineTuner::new(KnobSet::baseline(), cores);
    let window = 1.0; // seconds per control window

    println!("tick |  sustained | p99ms | rayon | sst_batch | ring");
    for t in 0..24 {
        let knobs = tuner.knobs();

        // ---- IN PRODUCTION: collect REAL counters over this window ----
        //   for each Stage, fill a RawStage:
        //     blocks         = blocks the stage completed this window
        //     stalls         = stall events
        //     queue_depth    = current inbound queue depth
        //     loss_pct       = measured packet loss (drives serve_redundancy)
        //     p99_latency_ms = spine.p99_ns(stage) as f64 / 1e6   (coordinated-omission)
        // Here we stand in the analytic model as the plant:
        let e = model_eval(&knobs, cores);
        let raw = [RawStage {
            blocks: (e.sustained_blk_s * window) as u64,
            p99_latency_ms: e.p99_ms,
            ..Default::default()
        }];
        // ---------------------------------------------------------------

        let next = tuner.tick(window, &raw);
        if t % 3 == 0 || t == 23 {
            println!(
                "{t:>4} | {:>9.0} | {:>5.1} | {:>5} | {:>9} | {:>4}",
                e.sustained_blk_s, e.p99_ms, next.rayon_threads, next.sst_batch, next.commit_ring_depth
            );
        }
    }

    let final_knobs = tuner.knobs();
    let final_eval = model_eval(&final_knobs, cores);
    println!(
        "\nconverged: {:.0} blk/s (target {}), p99 {:.1}ms",
        final_eval.sustained_blk_s, TARGET_BLK_S, final_eval.p99_ms
    );
    println!("apply to the live pipeline: {final_knobs:?}");
}
