//! online.rs — closes the control loop.
//!
//! Turns raw per-stage window counters into [`StageTelemetry`], steps the
//! [`AutoTuneController`], and hands back the new [`KnobSet`] to apply. The V7-INTEGRATE lane
//! wires real pipeline stages to this instead of re-implementing the glue: each stage
//! accumulates a [`RawStage`] over a control window, then one `tick()` per window drives tuning.

use crate::autotune::{AutoTuneController, KnobSet, StageTelemetry};

/// Raw per-stage counters a live stage accumulates over one control window.
/// `p99_latency_ms` is filled by the caller from `BackpressureSpine::p99_ns(stage) as f64 / 1e6`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RawStage {
    pub blocks: u64,
    pub stalls: u64,
    pub queue_depth: u32,
    pub loss_pct: f64,
    pub p99_latency_ms: f64,
}

/// Drives [`AutoTuneController`] from raw window samples. One per node.
pub struct OnlineTuner {
    ctrl: AutoTuneController,
}

impl OnlineTuner {
    pub fn new(initial: KnobSet, cores: u32) -> Self {
        Self {
            ctrl: AutoTuneController::new(initial, cores),
        }
    }

    /// Feed one control window of raw per-stage counters; returns the [`KnobSet`] to apply.
    /// `window_secs` = wall (or virtual) duration of the window, used to derive blk/s.
    pub fn tick(&mut self, window_secs: f64, raw: &[RawStage]) -> KnobSet {
        let dt = window_secs.max(1e-9);
        let stages: Vec<StageTelemetry> = raw
            .iter()
            .map(|r| StageTelemetry {
                blk_per_sec: r.blocks as f64 / dt,
                p99_latency_ms: r.p99_latency_ms,
                stalls: r.stalls,
                queue_depth: r.queue_depth,
                loss_pct: r.loss_pct,
            })
            .collect();
        self.ctrl.step(&stages)
    }

    pub fn knobs(&self) -> KnobSet {
        self.ctrl.knobs()
    }
    pub fn ticks(&self) -> u64 {
        self.ctrl.ticks()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::model_eval;

    /// Closed loop: use the analytic model as the "plant". Starting from the conservative
    /// baseline (~28k blk/s), the online controller must climb toward the 100k target and hold.
    #[test]
    fn online_loop_climbs_to_target_against_model() {
        let cores = 48;
        let mut tuner = OnlineTuner::new(KnobSet::baseline(), cores);
        let window = 1.0;
        for _ in 0..120 {
            let k = tuner.knobs();
            let e = model_eval(&k, cores);
            let raw = [RawStage {
                blocks: (e.sustained_blk_s * window) as u64,
                p99_latency_ms: e.p99_ms,
                ..Default::default()
            }];
            tuner.tick(window, &raw);
        }
        let final_eval = model_eval(&tuner.knobs(), cores);
        assert!(
            final_eval.sustained_blk_s >= 90_000.0,
            "online loop only reached {:.0} blk/s with knobs {:?}",
            final_eval.sustained_blk_s,
            tuner.knobs()
        );
    }
}
