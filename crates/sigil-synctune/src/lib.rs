//! sigil-synctune — V7-AUTOTUNE lane (agent: viktor-v7-coord)
//!
//! Control plane for the SIGIL v7 sync engine. Target: **100_000 blocks/sec sustained**.
//! Three pieces:
//!   * [`backpressure`] — the global token-bucket "backpressure spine" every pipeline
//!     stage (fetch / verify / commit / db-ingest) acquires from before processing blocks.
//!     Coordinated-omission aware (records intended-start vs actual-start, not just service).
//!   * [`autotune`] — an AIMD controller (per-knob, with anti-windup) that turns per-stage
//!     telemetry into knob setpoints: window / substream / rayon / sst-batch / ring / pid /
//!     serve-redundancy.
//!   * [`sweep`] — a deterministic virtual-time sweep that picks the config which *sustains*
//!     (not bursts) the target with bounded p99, using an analytic model of the known v7
//!     failure modes — so we can recommend a config with NO live node, then refine online.
//!
//! Everything is pure-std and driven by an injectable [`Clock`], so the whole crate is
//! deterministic and testable in virtual time (same discipline as flux-chronos).
//!
//! The serve-redundancy rule baked into the controller comes from real flux-chronos sweeps
//! run on 2026-06-26: redundancy=2 holds >=99.7% delivery up to 5% loss; escalate to 3 only
//! above 5% (replication cost rises ~linearly).

pub mod autotune;
pub mod backpressure;
pub mod online;
pub mod sweep;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Injectable monotonic clock: prod uses the OS clock, tests/sweeps use virtual time.
pub trait Clock: Send + Sync {
    fn now_nanos(&self) -> u64;
}

/// Real monotonic clock, anchored at construction.
pub struct RealClock {
    base: Instant,
}
impl RealClock {
    pub fn new() -> Self {
        Self { base: Instant::now() }
    }
}
impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}
impl Clock for RealClock {
    fn now_nanos(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }
}

/// Virtual clock for deterministic tests and sweeps. Advance it by hand.
pub struct VirtualClock {
    ns: AtomicU64,
}
impl VirtualClock {
    pub fn new() -> Self {
        Self { ns: AtomicU64::new(0) }
    }
    pub fn advance(&self, by_ns: u64) {
        self.ns.fetch_add(by_ns, Ordering::SeqCst);
    }
    pub fn advance_ms(&self, ms: u64) {
        self.advance(ms * 1_000_000);
    }
}
impl Default for VirtualClock {
    fn default() -> Self {
        Self::new()
    }
}
impl Clock for VirtualClock {
    fn now_nanos(&self) -> u64 {
        self.ns.load(Ordering::SeqCst)
    }
}

/// The block-per-second target the whole v7 effort is tuned to.
pub const TARGET_BLK_S: u32 = 100_000;

pub use autotune::{AutoTuneController, KnobSet, StageTelemetry};
pub use backpressure::{BackpressureSpine, CoLatency, RateGate, Stage};
pub use online::{OnlineTuner, RawStage};
pub use sweep::{model_eval, recommend_config, Eval, Sweep};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotune::{AutoTuneController, KnobSet, StageTelemetry};
    use crate::backpressure::BackpressureSpine;
    use crate::sweep::{model_eval, recommend_config};
    use std::sync::Arc;

    fn tel(blk: f64, p99: f64, stalls: u64, q: u32, loss: f64) -> StageTelemetry {
        StageTelemetry {
            blk_per_sec: blk,
            p99_latency_ms: p99,
            stalls,
            queue_depth: q,
            loss_pct: loss,
        }
    }

    #[test]
    fn token_bucket_rate_and_refill() {
        let clk = Arc::new(VirtualClock::new());
        let sp = BackpressureSpine::new(clk.clone(), TARGET_BLK_S, 1000, 4);
        // burst capacity = 1000 blocks
        assert!(sp.try_acquire(1000));
        assert!(!sp.try_acquire(1)); // drained
        clk.advance_ms(1); // 100k blk/s * 1ms = 100 blocks refilled
        assert!(sp.try_acquire(100));
        assert!(!sp.try_acquire(1));
    }

    #[test]
    fn wait_nanos_predicts_one_block() {
        let clk = Arc::new(VirtualClock::new());
        let sp = BackpressureSpine::new(clk, TARGET_BLK_S, 1, 1);
        assert!(sp.try_acquire(1)); // drain the single-block burst
        // one block at 100k blk/s = 10_000 ns
        let w = sp.wait_nanos(1);
        assert!((9_000..=11_000).contains(&w), "wait_nanos = {w}");
    }

    #[test]
    fn coordinated_omission_latency() {
        let clk = Arc::new(VirtualClock::new());
        let sp = BackpressureSpine::new(clk, TARGET_BLK_S, 1000, 4);
        // intended t=0, actually started t=5ms (queued), service 1ms -> CO latency ~6ms
        sp.record(1, 0, 5_000_000, 1_000_000);
        sp.record(1, 0, 5_000_000, 1_000_000);
        let p99 = sp.p99_ns(1);
        assert!((4_000_000..=8_000_000).contains(&p99), "p99 ns = {p99}");
    }

    #[test]
    fn controller_grows_when_healthy_shrinks_when_congested() {
        let mut c = AutoTuneController::new(KnobSet::baseline(), 48);
        let start = c.knobs().window_depth;
        for _ in 0..5 {
            c.step(&[tel(120_000.0, 20.0, 0, 4, 0.0)]); // healthy & above target
        }
        let peak = c.knobs().window_depth;
        assert!(peak > start, "window should grow when healthy ({start}->{peak})");
        for _ in 0..5 {
            c.step(&[tel(10_000.0, 900.0, 7, 4000, 1.0)]); // congested
        }
        assert!(c.knobs().window_depth < peak, "window should shrink when congested");
    }

    #[test]
    fn rategate_as_trait_object() {
        use crate::backpressure::{RateGate, Stage};
        let clk = Arc::new(VirtualClock::new());
        let sp: Arc<dyn RateGate> =
            Arc::new(BackpressureSpine::new(clk, TARGET_BLK_S, 500, Stage::COUNT));
        assert!(sp.admit(500)); // burst
        assert!(!sp.admit(1)); // drained
        assert_eq!(sp.admit_rate(), TARGET_BLK_S);
        assert_eq!(Stage::Ingest.idx(), 3);
    }

    #[test]
    fn redundancy_rule_matches_chronos_sweep() {
        let mut c = AutoTuneController::new(KnobSet::baseline(), 48);
        c.step(&[tel(90_000.0, 50.0, 0, 10, 3.0)]); // 3% loss
        assert_eq!(c.knobs().serve_redundancy, 2);
        c.step(&[tel(90_000.0, 50.0, 0, 10, 6.0)]); // 6% loss
        assert_eq!(c.knobs().serve_redundancy, 3);
    }

    #[test]
    fn sweep_finds_sustained_100k_config() {
        let (cfg, eval) = recommend_config(48);
        assert!(
            eval.sustained_blk_s >= (TARGET_BLK_S as f64) * 0.95,
            "sustained {} < target (cfg {:?})",
            eval.sustained_blk_s,
            cfg
        );
        assert!(eval.p99_ms <= 250.0, "p99 {}", eval.p99_ms);
        // the sweep must beat the conservative baseline
        let base = model_eval(&KnobSet::baseline(), 48);
        assert!(eval.sustained_blk_s > base.sustained_blk_s);
    }
}
