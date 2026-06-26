//! Demonstrate the closed online-refinement loop in virtual time: a congested pipeline that
//! the admission governor throttles, then recovers and lets the structural knobs grow.
//! Usage: `cargo run --example control_loop` (run via the flux wrapper, not raw cargo).

use std::sync::Arc;

use sigil_syncwire::{Stage, TelemetryBus, VirtualClock, TARGET_BLK_S};

fn main() {
    let clk = Arc::new(VirtualClock::new());
    let bus = TelemetryBus::new(clk.clone(), 1024, 48);

    println!("== sigil-syncwire control loop (target {TARGET_BLK_S} blk/s) ==");

    println!("\n-- phase 1: commit stage congested (5k blk/s, queue backing up) --");
    for t in 0..6 {
        bus.on_blocks(Stage::Fetch, 120_000);
        bus.on_blocks(Stage::Verify, 120_000);
        bus.on_blocks(Stage::Commit, 5_000);
        bus.on_blocks(Stage::Ingest, 5_000);
        bus.set_queue_depth(Stage::Commit, 8_000);
        bus.on_stall(Stage::Commit);
        clk.advance_ms(1_000);
        let (k, _) = bus.tick();
        println!(
            "  tick {t}: admit_rate={:>6} blk/s  window={:>4}  ring={:>4}",
            bus.admit_rate(),
            k.window_depth,
            k.commit_ring_depth
        );
    }

    println!("\n-- phase 2: recovered, all stages sustaining > target --");
    bus.set_queue_depth(Stage::Commit, 0);
    for t in 0..12 {
        for s in [Stage::Fetch, Stage::Verify, Stage::Commit, Stage::Ingest] {
            bus.on_blocks(s, 125_000);
        }
        clk.advance_ms(1_000);
        let (k, stats) = bus.tick();
        if t % 3 == 0 {
            println!(
                "  tick {t}: admit_rate={:>6} blk/s  window={:>4}  bottleneck={:>7.0} blk/s",
                bus.admit_rate(),
                k.window_depth,
                stats.iter().map(|s| s.blk_per_sec).fold(f64::INFINITY, f64::min)
            );
        }
    }

    println!("\nfinal admit_rate={} blk/s (ceiling = target)", bus.admit_rate());
}
