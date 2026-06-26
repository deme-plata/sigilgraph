//! Adaptive controller: per-stage telemetry -> knob setpoints.
//!
//! Per-knob AIMD (additive-increase / multiplicative-decrease) with anti-windup, plus the
//! empirically-derived serve-redundancy rule from the 2026-06-26 flux-chronos sweeps.

use crate::TARGET_BLK_S;

/// The tunable pipeline knobs. `Copy` so the controller hands out fresh setpoints cheaply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnobSet {
    /// In-flight block-pack window (fetch).
    pub window_depth: u32,
    /// Parallel WireGuard substreams (serve + fetch).
    pub substream_count: u32,
    /// Parallel verify workers (rayon).
    pub rayon_threads: u32,
    /// Blocks per sorted-SST bulk ingest batch (db).
    pub sst_batch: u32,
    /// Bounded-mpsc batch depth on commit.
    pub commit_ring_depth: u32,
    pub pid_kp: f64,
    pub pid_ki: f64,
    pub pid_kd: f64,
    /// Substream replication factor on the serve side.
    pub serve_redundancy: u32,
}

impl KnobSet {
    /// Conservative, known-safe starting point (~ the v6 defaults).
    pub fn baseline() -> Self {
        Self {
            window_depth: 256,
            substream_count: 8,
            rayon_threads: 4,
            sst_batch: 1024,
            commit_ring_depth: 128,
            pid_kp: 0.20,
            pid_ki: 0.02,
            pid_kd: 0.05,
            serve_redundancy: 2,
        }
    }
}

/// Per-stage telemetry sampled each control tick.
#[derive(Clone, Copy, Debug)]
pub struct StageTelemetry {
    pub blk_per_sec: f64,
    pub p99_latency_ms: f64,
    pub stalls: u64,
    pub queue_depth: u32,
    pub loss_pct: f64,
}

/// One numeric knob under AIMD control with single-step anti-windup.
#[derive(Clone, Copy)]
struct Aimd {
    value: f64,
    min: f64,
    max: f64,
    ai: f64, // additive increase
    md: f64, // multiplicative decrease (0..1)
    prev: f64,
    prev_score: f64,
}
impl Aimd {
    fn new(value: f64, min: f64, max: f64, ai: f64, md: f64) -> Self {
        Self {
            value,
            min,
            max,
            ai,
            md,
            prev: value,
            prev_score: f64::NEG_INFINITY,
        }
    }
    fn step(&mut self, congested: bool, score: f64) {
        if congested {
            // congestion response: multiplicative decrease (always shrink under load).
            self.prev = self.value;
            self.prev_score = score;
            self.value = (self.value * self.md).max(self.min);
        } else {
            // anti-windup: if the last (increase) move actually hurt the score, undo it
            // before increasing again — prevents runaway overshoot on the increase side.
            if score + 1e-9 < self.prev_score {
                self.value = self.prev;
            }
            self.prev = self.value;
            self.prev_score = score;
            self.value = (self.value + self.ai).min(self.max);
        }
    }
}

/// Turns live per-stage telemetry into knob setpoints, one control tick at a time.
pub struct AutoTuneController {
    target: f64,
    p99_budget_ms: f64,
    window: Aimd,
    substream: Aimd,
    rayon: Aimd,
    sst_batch: Aimd,
    ring: Aimd,
    redundancy: u32,
    knobs: KnobSet,
    ticks: u64,
}

impl AutoTuneController {
    pub fn new(initial: KnobSet, cores: u32) -> Self {
        let cores = cores.max(1) as f64;
        Self {
            target: TARGET_BLK_S as f64,
            p99_budget_ms: 250.0,
            window: Aimd::new(initial.window_depth as f64, 32.0, 8192.0, 32.0, 0.85),
            substream: Aimd::new(initial.substream_count as f64, 1.0, 32.0, 1.0, 0.85),
            rayon: Aimd::new(initial.rayon_threads as f64, 1.0, cores, 1.0, 0.90),
            sst_batch: Aimd::new(initial.sst_batch as f64, 128.0, 65536.0, 256.0, 0.85),
            ring: Aimd::new(initial.commit_ring_depth as f64, 16.0, 4096.0, 32.0, 0.85),
            redundancy: initial.serve_redundancy,
            knobs: initial,
            ticks: 0,
        }
    }

    /// Fold all stages into a scalar health score plus a congestion flag and the worst loss.
    /// Throughput is the *bottleneck* (min) stage; p99 over budget and stalls are penalties.
    fn score(&self, stages: &[StageTelemetry]) -> (f64, bool, f64) {
        if stages.is_empty() {
            return (0.0, true, 0.0);
        }
        let bottleneck = stages.iter().map(|s| s.blk_per_sec).fold(f64::INFINITY, f64::min);
        let max_p99 = stages.iter().map(|s| s.p99_latency_ms).fold(0.0, f64::max);
        let stalls: u64 = stages.iter().map(|s| s.stalls).sum();
        let max_loss = stages.iter().map(|s| s.loss_pct).fold(0.0, f64::max);

        let throughput_ratio = bottleneck / self.target;
        let latency_penalty = (max_p99 / self.p99_budget_ms).max(1.0);
        let stall_penalty = 1.0 + stalls as f64;
        let score = throughput_ratio / (latency_penalty * stall_penalty);

        let congested =
            bottleneck < self.target * 0.95 || max_p99 > self.p99_budget_ms || stalls > 0;
        (score, congested, max_loss)
    }

    /// One control tick: telemetry in, fresh knob setpoints out.
    pub fn step(&mut self, stages: &[StageTelemetry]) -> KnobSet {
        self.ticks += 1;
        let (score, congested, max_loss) = self.score(stages);

        self.window.step(congested, score);
        self.substream.step(congested, score);
        self.rayon.step(congested, score);
        self.sst_batch.step(congested, score);
        self.ring.step(congested, score);

        // Empirically-derived serve-redundancy rule (flux-chronos sweep, 2026-06-26):
        //   redundancy=2 holds >=99.7% delivery up to 5% loss; escalate to 3 only above 5%.
        self.redundancy = if max_loss > 5.0 { 3 } else { 2 };

        self.knobs = KnobSet {
            window_depth: self.window.value.round() as u32,
            substream_count: self.substream.value.round() as u32,
            rayon_threads: self.rayon.value.round() as u32,
            sst_batch: self.sst_batch.value.round() as u32,
            commit_ring_depth: self.ring.value.round() as u32,
            serve_redundancy: self.redundancy,
            ..self.knobs
        };
        self.knobs
    }

    pub fn knobs(&self) -> KnobSet {
        self.knobs
    }
    pub fn ticks(&self) -> u64 {
        self.ticks
    }
}
