//! V7-GATE (agent v7-gate) — the FAST, deterministic pre-check half of the sigil-top
//! v7.0.0 100k-sustained acceptance gate. Pure analytic, NO live node, builds in seconds.
//!
//! ## Two layers of honesty (why this crate is necessary but NOT sufficient)
//!   1. **Oracle pre-check** (this crate): `sigil_synctune::recommend_config` + `model_eval`
//!      give a deterministic sustained-blk/s + p99 prediction. This is a fast tripwire: if
//!      even the *optimistic analytic model* can't reach 100k, fail instantly and don't waste
//!      a heavy build. The gate threshold matches `Eval::sustains`: sustained ≥ target*0.999
//!      AND p99 ≤ budget.
//!   2. **Spine coordinated-omission probe** (this crate): drives the real
//!      `BackpressureSpine` in virtual time at the target rate and reads `p99_ns(Stage::*)`
//!      per stage (0..3 = Fetch/Verify/Commit/Ingest), so the per-stage latency-under-load
//!      surface the live lanes use is exercised and gated, and the bottleneck stage is NAMED.
//!
//! ## ⚠️ The blind spot this gate refuses to hide (the gatekeeper's core finding)
//!   `sigil_synctune::sweep::model_eval` models FOUR stages — fetch, verify, commit, ingest —
//!   and has **NO term for the pure-Rust `ruzstd` inflate/decode CPU cost**, which the live
//!   measurements (#616: "transport inflate(ruzstd) 73k = LANE-3's codec wall") and the
//!   EMPIRICAL bench (`sync_100k_e2e.rs`) both show is the *actual* client bottleneck at
//!   ~70–80k blk/s — BELOW 100k. So the oracle can (and does, at the recommended config)
//!   report ~106k GREEN while the real client path is RED. **Oracle GREEN ≠ ship.** The full
//!   gate ANDs: oracle pre-check ∧ empirical real-CPU bench ∧ real 2-node demo. Only all-green
//!   tags v7.0.0. This crate prints that caveat on every run so it can never be quoted alone.

use sigil_synctune::{
    model_eval, recommend_config, BackpressureSpine, KnobSet, Stage, VirtualClock, TARGET_BLK_S,
};
use std::sync::Arc;

/// p99 budget for the whole-pipeline oracle check (matches `Sweep` default).
pub const P99_BUDGET_MS: f64 = 250.0;
/// Per-stage coordinated-omission p99 budget (ns). A stage that queues past this under load
/// is a stall, not steady state. 150 ms is generous; the recommended config models ~9 ms.
pub const STAGE_P99_BUDGET_NS: u64 = 150_000_000;

#[derive(Clone, Copy, Debug)]
pub struct StageRate {
    pub name: &'static str,
    pub blk_s: f64,
}

/// Mirror of `sigil_synctune::sweep::model_eval`'s per-stage capacity formulas, so the gate
/// can NAME the bottleneck stage (the public `Eval` returns only the min). Kept in lockstep
/// with the canonical model by `tests::mirror_matches_model_sustained` — if viktor-v7-coord
/// changes the model, that test fails and this is updated. **Note the deliberate absence of
/// an inflate stage — that absence IS the finding the empirical bench exists to expose.**
pub fn model_stage_rates(k: &KnobSet, cores: u32) -> [StageRate; 4] {
    let cores = cores.max(1) as f64;
    let ss = k.substream_count as f64;
    let bufferbloat = if ss > 16.0 { 1.0 + (ss - 16.0) * 0.06 } else { 1.0 };
    let window_gain = (1.0 + (k.window_depth as f64 / 2048.0)).min(2.0);
    let fetch = 12_000.0 * ss.min(16.0) * window_gain / bufferbloat;

    let rt = k.rayon_threads as f64;
    let false_sharing = if rt > cores { 1.0 + (rt - cores) * 0.15 } else { 1.0 };
    let verify = 9_000.0 * rt.min(cores) / false_sharing;

    let rd = k.commit_ring_depth as f64;
    let commit = 130_000.0 * (rd / (rd + 64.0));

    let sb = k.sst_batch as f64;
    let compaction_stall = if sb > 16_384.0 { 16_384.0 / sb } else { 1.0 };
    let ingest = 140_000.0 * (sb / (sb + 4096.0)) * compaction_stall;

    [
        StageRate { name: "fetch", blk_s: fetch },
        StageRate { name: "verify", blk_s: verify },
        StageRate { name: "commit", blk_s: commit },
        StageRate { name: "ingest", blk_s: ingest },
    ]
}

#[derive(Clone, Debug)]
pub struct OracleVerdict {
    pub knobs: KnobSet,
    pub sustained_blk_s: f64,
    pub p99_ms: f64,
    pub baseline_blk_s: f64,
    pub bottleneck: &'static str,
    pub bottleneck_blk_s: f64,
    pub green: bool,
}

/// The fast deterministic pre-check. `cores` = host cores (Epsilon = 48).
pub fn oracle_verdict(cores: u32) -> OracleVerdict {
    let target = TARGET_BLK_S as f64;
    let (knobs, eval) = recommend_config(cores);
    let baseline = model_eval(&KnobSet::baseline(), cores);
    let rates = model_stage_rates(&knobs, cores);
    let bn = rates
        .iter()
        .fold(rates[0], |a, b| if b.blk_s < a.blk_s { *b } else { a });
    OracleVerdict {
        knobs,
        sustained_blk_s: eval.sustained_blk_s,
        p99_ms: eval.p99_ms,
        baseline_blk_s: baseline.sustained_blk_s,
        bottleneck: bn.name,
        bottleneck_blk_s: bn.blk_s,
        green: eval.sustains(target, P99_BUDGET_MS),
    }
}

#[derive(Clone, Debug)]
pub struct StageCo {
    pub stage: &'static str,
    pub idx: usize,
    pub p99_ns: u64,
    pub within_budget: bool,
}

#[derive(Clone, Debug)]
pub struct SpineVerdict {
    pub stages: Vec<StageCo>,
    pub worst_stage: &'static str,
    pub worst_p99_ns: u64,
    pub green: bool,
}

/// Drive the REAL `BackpressureSpine` in virtual time at the target rate and read the
/// coordinated-omission p99 per stage. Each stage's per-batch service time is derived from the
/// model stage capacity at the recommended knobs; a stage slower than the target accumulates
/// queueing delay (intended vs actual-start) → its CO p99 climbs → caught here. This exercises
/// the exact `record()`/`p99_ns()` surface the live lanes use, deterministically.
pub fn spine_co_probe(cores: u32, batches: u64, batch: u32) -> SpineVerdict {
    let (knobs, _) = recommend_config(cores);
    let rates = model_stage_rates(&knobs, cores);
    let clk = Arc::new(VirtualClock::new());
    let spine = BackpressureSpine::new(clk.clone(), TARGET_BLK_S, batch.max(1) * 4, Stage::COUNT);

    let target = TARGET_BLK_S as f64;
    let intended_interval_ns = (batch as f64 / target * 1e9) as u64; // ns between batch arrivals
    // per-stage service time for one batch (ns), from the model capacity.
    let svc_ns: Vec<u64> = rates
        .iter()
        .map(|r| (batch as f64 / r.blk_s * 1e9) as u64)
        .collect();

    // Each stage advances its own backlog clock; if service > interval, actual-start drifts
    // later than intended → coordinated-omission latency grows (the honest signal).
    let mut stage_clock_ns = vec![0u64; Stage::COUNT];
    for b in 0..batches {
        let intended = b * intended_interval_ns;
        for s in 0..Stage::COUNT {
            let actual_start = stage_clock_ns[s].max(intended);
            spine.record(s, intended, actual_start, svc_ns[s]);
            stage_clock_ns[s] = actual_start + svc_ns[s];
        }
        clk.advance(intended_interval_ns);
    }

    let names = ["fetch", "verify", "commit", "ingest"];
    let mut stages = Vec::with_capacity(Stage::COUNT);
    for s in 0..Stage::COUNT {
        let p99 = spine.p99_ns(s);
        stages.push(StageCo {
            stage: names[s],
            idx: s,
            p99_ns: p99,
            within_budget: p99 <= STAGE_P99_BUDGET_NS,
        });
    }
    let worst = stages
        .iter()
        .fold(stages[0].clone(), |a, b| if b.p99_ns > a.p99_ns { b.clone() } else { a });
    let green = stages.iter().all(|s| s.within_budget);
    SpineVerdict {
        stages,
        worst_stage: worst.stage,
        worst_p99_ns: worst.p99_ns,
        green,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: our local stage formulas must reproduce the canonical model's sustained
    /// (= min stage). If viktor-v7-coord changes `sweep::model_eval`, this fails → update
    /// `model_stage_rates`.
    #[test]
    fn mirror_matches_model_sustained() {
        for cores in [8u32, 16, 48] {
            let (k, e) = recommend_config(cores);
            let rates = model_stage_rates(&k, cores);
            let min = rates.iter().fold(f64::INFINITY, |a, r| a.min(r.blk_s));
            assert!(
                (min - e.sustained_blk_s).abs() < 1.0,
                "cores={cores}: mirror min {min} != model sustained {}",
                e.sustained_blk_s
            );
        }
    }

    #[test]
    fn oracle_green_at_48c() {
        let v = oracle_verdict(48);
        assert!(v.green, "oracle should green at 48c: {v:?}");
        assert!(v.sustained_blk_s >= (TARGET_BLK_S as f64) * 0.999);
        assert!(v.sustained_blk_s > v.baseline_blk_s, "must beat baseline");
    }

    #[test]
    fn spine_co_healthy_at_recommended_config() {
        // At the recommended config every model stage clears the target, so CO p99 stays in
        // budget — proves the probe doesn't false-RED a healthy config.
        let v = spine_co_probe(48, 5_000, 128);
        assert!(v.green, "healthy spine should be in budget: {v:?}");
    }

    #[test]
    fn spine_co_reds_a_starved_stage() {
        // Force a starved pipeline: with batch large relative to a throttled spine the CO
        // latency must climb and be caught — the probe must be ABLE to fail.
        let clk = Arc::new(VirtualClock::new());
        let spine = BackpressureSpine::new(clk, 1_000, 1, Stage::COUNT);
        // intended=0 but actual-start far later (queued) → huge CO latency on stage 2.
        for _ in 0..1000 {
            spine.record(Stage::Commit.idx(), 0, 1_000_000_000, 1_000_000);
        }
        assert!(spine.p99_ns(Stage::Commit.idx()) > STAGE_P99_BUDGET_NS);
    }
}
