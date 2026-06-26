//! sigil-v7-gate — FAST oracle + spine coordinated-omission pre-check for the v7.0.0 cut.
//! Prints machine-parseable GATE_RESULT lines (the release-gate script greps them) and exits
//! 0 (GREEN) / 1 (RED). This is the FAST tripwire; the empirical real-CPU bench
//! (sync_100k_e2e.rs) + the 2-node demo are the authoritative confirmation the gate ANDs.
//!
//! Run:  sigil-v7-gate [cores]    (default: detected nproc, else 48)

use sigil_v7_gate::{oracle_verdict, spine_co_probe, STAGE_P99_BUDGET_NS};
use sigil_synctune::TARGET_BLK_S;

fn main() {
    let cores: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(48));
    let target = TARGET_BLK_S as f64;

    eprintln!("================ V7-GATE pre-check (oracle + spine CO) @ {cores} cores ================");

    // ── 1. Oracle pre-check (deterministic analytic) ──
    let o = oracle_verdict(cores);
    eprintln!(
        "ORACLE: sustained={:.0} blk/s  p99={:.1} ms  baseline={:.0}  bottleneck(model)={}@{:.0}",
        o.sustained_blk_s, o.p99_ms, o.baseline_blk_s, o.bottleneck, o.bottleneck_blk_s
    );
    eprintln!(
        "ORACLE knobs: window={} substreams={} rayon={} sst_batch={} ring={} redundancy={}",
        o.knobs.window_depth, o.knobs.substream_count, o.knobs.rayon_threads,
        o.knobs.sst_batch, o.knobs.commit_ring_depth, o.knobs.serve_redundancy
    );
    println!(
        "GATE_RESULT stage=oracle sustained_blk_s={:.0} target={:.0} p99_ms={:.1} verdict={}",
        o.sustained_blk_s, target, o.p99_ms, if o.green { "GREEN" } else { "RED" }
    );

    // ── 2. Spine coordinated-omission probe (per-stage p99) ──
    let s = spine_co_probe(cores, 20_000, 128);
    for st in &s.stages {
        eprintln!(
            "  spine CO stage[{}]={:<7} p99={:>10} ns  budget={} ns  {}",
            st.idx, st.stage, st.p99_ns, STAGE_P99_BUDGET_NS,
            if st.within_budget { "OK" } else { "OVER ✗" }
        );
        println!(
            "GATE_RESULT stage=spine:{} p99_ns={} budget_ns={} verdict={}",
            st.stage, st.p99_ns, STAGE_P99_BUDGET_NS,
            if st.within_budget { "GREEN" } else { "RED" }
        );
    }
    eprintln!("  spine worst stage = {} @ {} ns", s.worst_stage, s.worst_p99_ns);

    let green = o.green && s.green;
    println!(
        "GATE_RESULT stage=precheck:OVERALL verdict={}",
        if green { "GREEN" } else { "RED" }
    );

    // The caveat that must travel with every oracle GREEN.
    eprintln!("--------------------------------------------------------------------------------");
    eprintln!("⚠️  ORACLE GREEN ≠ SHIP. model_eval has NO inflate stage; live #616 + the empirical");
    eprintln!("    bench show pure-Rust ruzstd inflate ~70–80k blk/s < 100k. The v7.0.0 tag also");
    eprintln!("    requires sync_100k_e2e.rs (empirical real-CPU) AND the 2-node demo GREEN.");
    eprintln!("================ V7-GATE pre-check verdict: {} ================", if green { "GREEN" } else { "RED" });

    std::process::exit(if green { 0 } else { 1 });
}
