//! Deterministic virtual-time sweep that picks a config sustaining the target with bounded p99.
//!
//! The evaluator is pluggable. The built-in [`model_eval`] encodes the known v7 failure modes
//! (DB-ingest compaction wall, rayon false-sharing past cores, substream bufferbloat, commit-ring
//! contention) so we can recommend a config with NO live node — then the online
//! [`crate::AutoTuneController`] refines it against real telemetry.

use crate::autotune::KnobSet;
use crate::TARGET_BLK_S;

#[derive(Clone, Copy, Debug)]
pub struct Eval {
    pub sustained_blk_s: f64,
    pub p99_ms: f64,
}

impl Eval {
    /// "Good" = sustains >=99.9% of target with p99 within budget.
    pub fn sustains(&self, target: f64, p99_budget_ms: f64) -> bool {
        self.sustained_blk_s >= target * 0.999 && self.p99_ms <= p99_budget_ms
    }
    /// Scalar fitness: throughput (capped just above target) times latency headroom.
    pub fn score(&self, target: f64, p99_budget_ms: f64) -> f64 {
        let t = (self.sustained_blk_s / target).min(1.05);
        let l = (p99_budget_ms / self.p99_ms.max(1.0)).min(1.0);
        t * l
    }
}

/// Analytic pipeline model. `cores` is the host core count (epsilon = 48).
/// Each stage has a capacity in blk/s as a function of the knobs; sustained throughput is the
/// bottleneck (slowest) stage, and p99 grows as the target presses against that bottleneck.
pub fn model_eval(k: &KnobSet, cores: u32) -> Eval {
    let cores = cores.max(1) as f64;

    // FETCH: scales with substreams * window, saturating; bufferbloat penalty past ~16 substreams
    // (DeepSeek failure mode #1: WireGuard head-of-line blocking).
    let ss = k.substream_count as f64;
    let bufferbloat = if ss > 16.0 { 1.0 + (ss - 16.0) * 0.06 } else { 1.0 };
    let window_gain = (1.0 + (k.window_depth as f64 / 2048.0)).min(2.0);
    let fetch = 12_000.0 * ss.min(16.0) * window_gain / bufferbloat;

    // VERIFY: scales with rayon threads up to cores; false-sharing penalty past cores
    // (DeepSeek failure mode #2).
    let rt = k.rayon_threads as f64;
    let false_sharing = if rt > cores { 1.0 + (rt - cores) * 0.15 } else { 1.0 };
    let verify = 9_000.0 * rt.min(cores) / false_sharing;

    // COMMIT: bounded-mpsc batch; tiny batches livelock on CAS (DeepSeek failure mode #3),
    // large depth saturates toward the ceiling.
    let rd = k.commit_ring_depth as f64;
    let commit = 130_000.0 * (rd / (rd + 64.0));

    // DB-INGEST: the DeepSeek-identified NEW wall. Sorted-SST bulk throughput rises with batch
    // size but compaction stalls past a ~16k sweet spot.
    let sb = k.sst_batch as f64;
    let compaction_stall = if sb > 16_384.0 { 16_384.0 / sb } else { 1.0 };
    let ingest = 140_000.0 * (sb / (sb + 4096.0)) * compaction_stall;

    let sustained = [fetch, verify, commit, ingest]
        .into_iter()
        .fold(f64::INFINITY, f64::min);

    // p99: base service + queueing that explodes as the target presses on the bottleneck,
    // plus a ring-livelock latency spike when the commit ring is too shallow.
    let target = TARGET_BLK_S as f64;
    let pressure = (target / sustained).max(1.0);
    let ring_spike = if k.commit_ring_depth < 32 { 200.0 } else { 0.0 };
    let p99_ms = 8.0 * pressure * pressure + ring_spike + (k.window_depth as f64 / 256.0);

    Eval {
        sustained_blk_s: sustained,
        p99_ms,
    }
}

/// Coordinate-descent sweep over the knob space. Deterministic (no RNG).
pub struct Sweep {
    pub cores: u32,
    pub target: f64,
    pub p99_budget_ms: f64,
}

impl Sweep {
    pub fn new(cores: u32) -> Self {
        Self {
            cores,
            target: TARGET_BLK_S as f64,
            p99_budget_ms: 250.0,
        }
    }

    /// Greedy coordinate descent: each round, try scaling each knob by a set of factors and
    /// keep any move that improves the fitness score. Stop when a round finds no improvement.
    pub fn run<F: Fn(&KnobSet, u32) -> Eval>(&self, eval: F) -> (KnobSet, Eval) {
        let mut best = KnobSet::baseline();
        let mut best_eval = eval(&best, self.cores);
        let mut best_score = best_eval.score(self.target, self.p99_budget_ms);

        let factors = [0.5_f64, 0.75, 1.5, 2.0];
        for _round in 0..16 {
            let mut improved = false;
            for knob_idx in 0..5 {
                for &f in &factors {
                    let mut cand = best;
                    match knob_idx {
                        0 => {
                            cand.window_depth =
                                ((best.window_depth as f64 * f).round() as u32).clamp(32, 8192)
                        }
                        1 => {
                            cand.substream_count =
                                ((best.substream_count as f64 * f).round() as u32).clamp(1, 32)
                        }
                        2 => {
                            cand.rayon_threads = ((best.rayon_threads as f64 * f).round() as u32)
                                .clamp(1, self.cores)
                        }
                        3 => {
                            cand.sst_batch =
                                ((best.sst_batch as f64 * f).round() as u32).clamp(128, 65536)
                        }
                        4 => {
                            cand.commit_ring_depth =
                                ((best.commit_ring_depth as f64 * f).round() as u32).clamp(16, 4096)
                        }
                        _ => {}
                    }
                    let e = eval(&cand, self.cores);
                    let s = e.score(self.target, self.p99_budget_ms);
                    if s > best_score + 1e-9 {
                        best = cand;
                        best_eval = e;
                        best_score = s;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
        (best, best_eval)
    }
}

/// Convenience: run the built-in model sweep and return the recommended config for `cores`.
pub fn recommend_config(cores: u32) -> (KnobSet, Eval) {
    Sweep::new(cores).run(model_eval)
}
