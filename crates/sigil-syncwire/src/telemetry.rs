//! The telemetry bus: the LIVE wiring that closes sigil-synctune's [`OnlineTuner`] over the
//! real pipeline.
//!
//! It does **not** re-implement the control loop — `OnlineTuner` (sigil-synctune, commit
//! 9224cf3) already turns `RawStage` window samples into a [`KnobSet`]. This bus supplies the
//! two things `OnlineTuner` deliberately leaves to the integration layer:
//!   1. a **cheap, lock-free, cache-line-padded live counter collector** the real stages hit on
//!      their hot path (`on_blocks` / `on_stall` / `set_queue_depth` / `record_service` /
//!      `set_loss`), snapshotted into a [`RawStage`] per control window, and
//!   2. the **admission-rate governor** — `OnlineTuner` tunes pipeline *shape* (the structural
//!      knobs) but never the *pace*; the spine's admission rate is a separate actuator that
//!      prevents producer overrun → OOM. This is an AIMD loop on `spine.set_rate`.
//!
//! Each [`TelemetryBus::tick`] folds the counters into one `RawStage` per stage, runs the
//! admission governor, drives `OnlineTuner::tick`, and returns the fresh [`KnobSet`] plus a
//! per-stage [`StageStat`] (REAL measured blk/s — the number the v7 TUI shows climbing, never
//! faked-to-tip).
//!
//! ### Coordinated-omission awareness
//! Throughput alone hides stalls: a stage that freezes 200ms then bursts still reports a healthy
//! mean. We never trust blk/s alone — p99 comes from the shared spine's CO histogram
//! (`spine.p99_ns`, intended-start vs actual-start + service), and any stage over the p99 budget
//! marks the window congested regardless of throughput.
//!
//! ### False-sharing mitigation
//! Each stage's counters live in a [`Padded`] cell aligned to a 64-byte cache line, so four
//! stages hammering their own counters on four cores don't ping-pong one line.
//!
//! ### Two loops, one congested bit — anti-coupling guard
//! `OnlineTuner` (shape) and the governor (pace) share the same `congested` signal, which risks
//! a coupled limit cycle (DeepSeek control review, 2026-06-26): the governor throttles → load
//! relief reads as "healthy" to the tuner → the tuner grows capacity → overshoot when the
//! governor ramps pace back → congestion → repeat. The guard breaks it by role/timescale
//! separation: only let the tuner grow at *full pace* (rate back at target) or shrink under
//! *real* congestion; HOLD during artificial relief.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sigil_synctune::{
    BackpressureSpine, Clock, KnobSet, OnlineTuner, RateGate, RawStage, Stage, TARGET_BLK_S,
};

/// p99 latency budget (ms) above which a stage is congested no matter its throughput.
/// Matches the synctune controller's internal budget so both loops agree on "congested".
const P99_BUDGET_MS: f64 = 250.0;
/// Throughput fraction of target below which the bottleneck stage is congested. Mirrors
/// `AutoTuneController::score`'s 0.95 so the governor and the knob-tuner never disagree about
/// the sign of the control action (the anti-oscillation invariant).
const HEALTHY_FRAC: f64 = 0.95;

/// Cache-line-padded cell. Aligning to 64 bytes keeps each stage's hot counters on their own
/// line so concurrent `fetch_add`s from different stages don't false-share.
#[repr(align(64))]
struct Padded<T>(T);
impl<T> std::ops::Deref for Padded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Lock-free counters one stage writes and the bus reads. `blocks`/`stalls` are monotonic
/// (the bus diffs them per tick); `queue_depth`/`loss_ppm` are gauges (last-write-wins).
struct StageCounters {
    blocks: AtomicU64,
    stalls: AtomicU64,
    queue_depth: AtomicU64,
    loss_ppm: AtomicU64,
}
impl StageCounters {
    fn new() -> Self {
        Self {
            blocks: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            queue_depth: AtomicU64::new(0),
            loss_ppm: AtomicU64::new(0),
        }
    }
}

/// AIMD governor for the admission *rate ceiling*. Additive-increase back toward the tuned
/// target when healthy; multiplicative-decrease under congestion. Clamped in `[floor, target]`
/// so it can only ever throttle below target to relieve overrun — never exceed it (windup-safe).
struct AdmissionGovernor {
    rate: f64,
    floor: f64,
    ceil: f64,
    ai: f64,
    md: f64,
}
impl AdmissionGovernor {
    fn new(target: u32) -> Self {
        let t = target as f64;
        Self {
            rate: t,
            floor: (t * 0.10).max(1.0), // never starve below 10% of target
            ceil: t,
            ai: (t * 0.05).max(1.0), // additive step = 5% of target
            md: 0.85,                // matches synctune's window/ring decrease factor
        }
    }
    fn step(&mut self, congested: bool) -> u32 {
        self.rate = if congested {
            (self.rate * self.md).max(self.floor)
        } else {
            (self.rate + self.ai).min(self.ceil)
        };
        self.rate.round() as u32
    }
}

/// Mutable control state, behind one lock so a tick is atomic w.r.t. the lock-free counters.
struct ControlState {
    tuner: OnlineTuner,
    governor: AdmissionGovernor,
    last_ns: u64,
    last_blocks: [u64; Stage::COUNT],
    last_stalls: [u64; Stage::COUNT],
    last_knobs: KnobSet,
}

/// One snapshot of a stage's derived telemetry, returned by [`TelemetryBus::tick`] alongside the
/// knobs for logging / the v7 TUI honesty readout (REAL measured blk/s, not faked-to-tip).
#[derive(Clone, Copy, Debug)]
pub struct StageStat {
    pub stage: Stage,
    pub blk_per_sec: f64,
    pub p99_ms: f64,
    pub stalls: u64,
    pub queue_depth: u32,
}

const STAGES: [Stage; Stage::COUNT] = [Stage::Fetch, Stage::Verify, Stage::Commit, Stage::Ingest];

/// The integration bus. Generic over the clock so prod uses [`sigil_synctune::RealClock`] and
/// tests/sweeps use [`sigil_synctune::VirtualClock`] — the closed loop is fully deterministic.
pub struct TelemetryBus<C: Clock> {
    clock: Arc<C>,
    spine: Arc<BackpressureSpine<C>>,
    counters: [Padded<StageCounters>; Stage::COUNT],
    ctrl: Mutex<ControlState>,
}

impl<C: Clock + 'static> TelemetryBus<C> {
    /// Build the bus and the single shared spine from one clock instance. `burst_blocks` = the
    /// spine's bucket capacity; `cores` sizes the rayon-thread knob ceiling.
    pub fn new(clock: Arc<C>, burst_blocks: u32, cores: u32) -> Self {
        let spine = Arc::new(BackpressureSpine::new(
            clock.clone(),
            TARGET_BLK_S,
            burst_blocks,
            Stage::COUNT,
        ));
        let now = clock.now_nanos();
        let knobs = KnobSet::baseline();
        Self {
            clock,
            spine,
            counters: std::array::from_fn(|_| Padded(StageCounters::new())),
            ctrl: Mutex::new(ControlState {
                tuner: OnlineTuner::new(knobs, cores),
                governor: AdmissionGovernor::new(TARGET_BLK_S),
                last_ns: now,
                last_blocks: [0; Stage::COUNT],
                last_stalls: [0; Stage::COUNT],
                last_knobs: knobs,
            }),
        }
    }

    /// The shared admission gate stages acquire from (serve + fetch + commit + ingest).
    /// Cheap `Arc` clone of the spine. This is the `Arc<dyn RateGate>` v7-supply's serve loop
    /// and v7-ingest's `Pass2Sink::from_env_with_gate` consume — one bucket caps the whole
    /// pipeline.
    pub fn gate(&self) -> Arc<dyn RateGate> {
        self.spine.clone()
    }

    /// The CONCRETE shared spine handle. Needed by stages that record coordinated-omission
    /// samples (`BackpressureSpine::record` is not on the `RateGate` trait — e.g. v7-ingest
    /// wiring `record(Stage::Ingest, …)` inside its SST install). Same bucket as [`gate`].
    pub fn spine(&self) -> Arc<BackpressureSpine<C>> {
        self.spine.clone()
    }

    /// Current admission rate ceiling (blocks/sec) the governor has settled on.
    pub fn admit_rate(&self) -> u32 {
        self.spine.rate()
    }

    // ---- cheap, lock-free signals stages emit on their hot path ----

    /// A stage processed `n` blocks. Monotonic; the bus diffs it into a per-tick rate.
    #[inline]
    pub fn on_blocks(&self, stage: Stage, n: u64) {
        self.counters[stage.idx()].blocks.fetch_add(n, Ordering::Relaxed);
    }

    /// A stage stalled (e.g. starved on its input or blocked on the gate). Monotonic.
    #[inline]
    pub fn on_stall(&self, stage: Stage) {
        self.counters[stage.idx()].stalls.fetch_add(1, Ordering::Relaxed);
    }

    /// Last-observed queue depth feeding `stage` (gauge).
    #[inline]
    pub fn set_queue_depth(&self, stage: Stage, depth: u32) {
        self.counters[stage.idx()]
            .queue_depth
            .store(depth as u64, Ordering::Relaxed);
    }

    /// Last-observed packet/block loss percentage for `stage` (gauge); drives serve redundancy.
    #[inline]
    pub fn set_loss(&self, stage: Stage, pct: f64) {
        let ppm = (pct.max(0.0) * 10_000.0).round() as u64;
        self.counters[stage.idx()].loss_ppm.store(ppm, Ordering::Relaxed);
    }

    /// Record a coordinated-omission service sample into the shared spine histogram: when work
    /// was *intended* to start vs when it *actually* started, plus service time. The bus reads
    /// p99 back from the spine at tick time. This is what makes latency-under-load visible
    /// instead of hidden by throughput.
    #[inline]
    pub fn record_service(
        &self,
        stage: Stage,
        intended_ns: u64,
        actual_start_ns: u64,
        service_ns: u64,
    ) {
        self.spine
            .record(stage.idx(), intended_ns, actual_start_ns, service_ns);
    }

    /// One control tick. Returns the fresh [`KnobSet`] to fan out to stages, plus a per-stage
    /// [`StageStat`] snapshot (REAL measured blk/s). Idempotent if no time has passed (returns
    /// the last knobs without re-stepping, so a too-eager caller can't divide by zero or spin
    /// the tuner on stale deltas).
    pub fn tick(&self) -> (KnobSet, [StageStat; Stage::COUNT]) {
        let now = self.clock.now_nanos();
        let mut st = self.ctrl.lock().unwrap();

        let elapsed_ns = now.saturating_sub(st.last_ns);
        if elapsed_ns == 0 {
            let stats = self.idle_snapshot();
            return (st.last_knobs, stats);
        }
        let elapsed_s = elapsed_ns as f64 / 1e9;

        // Fold the lock-free counters into one RawStage per stage (the live-counter -> sample
        // step OnlineTuner expects the integration layer to do).
        let mut raw = [RawStage::default(); Stage::COUNT];
        for i in 0..Stage::COUNT {
            let c = &self.counters[i];
            let blocks = c.blocks.load(Ordering::Relaxed);
            let stalls = c.stalls.load(Ordering::Relaxed);
            let d_blocks = blocks.saturating_sub(st.last_blocks[i]);
            let d_stalls = stalls.saturating_sub(st.last_stalls[i]);
            st.last_blocks[i] = blocks;
            st.last_stalls[i] = stalls;
            raw[i] = RawStage {
                blocks: d_blocks,
                stalls: d_stalls,
                queue_depth: c.queue_depth.load(Ordering::Relaxed) as u32,
                loss_pct: c.loss_ppm.load(Ordering::Relaxed) as f64 / 10_000.0,
                p99_latency_ms: self.spine.p99_ns(i) as f64 / 1e6,
            };
        }

        // Single shared congestion definition (mirrors AutoTuneController::score) so the
        // structural knobs and the admission governor always move in the same direction.
        let target = TARGET_BLK_S as f64;
        let bottleneck = raw
            .iter()
            .map(|r| r.blocks as f64 / elapsed_s)
            .fold(f64::INFINITY, f64::min);
        let max_p99 = raw.iter().map(|r| r.p99_latency_ms).fold(0.0, f64::max);
        let total_stalls: u64 = raw.iter().map(|r| r.stalls).sum();
        let congested =
            bottleneck < target * HEALTHY_FRAC || max_p99 > P99_BUDGET_MS || total_stalls > 0;

        // Pace actuator (Loop B): governor sets the spine admission ceiling.
        let new_rate = st.governor.step(congested);
        self.spine.set_rate(new_rate);

        // Shape actuator (Loop A) with the anti-coupling guard: only let OnlineTuner move at
        // full pace (rate restored to target) or shrink under real congestion; HOLD during
        // artificial relief so it never grows capacity into an about-to-overshoot pipeline.
        let knobs = if congested || new_rate >= TARGET_BLK_S {
            st.tuner.tick(elapsed_s, &raw)
        } else {
            st.last_knobs
        };

        st.last_ns = now;
        st.last_knobs = knobs;

        let stats = std::array::from_fn(|i| StageStat {
            stage: STAGES[i],
            blk_per_sec: raw[i].blocks as f64 / elapsed_s,
            p99_ms: raw[i].p99_latency_ms,
            stalls: raw[i].stalls,
            queue_depth: raw[i].queue_depth,
        });
        (knobs, stats)
    }

    fn idle_snapshot(&self) -> [StageStat; Stage::COUNT] {
        std::array::from_fn(|i| StageStat {
            stage: STAGES[i],
            blk_per_sec: 0.0,
            p99_ms: self.spine.p99_ns(i) as f64 / 1e6,
            stalls: 0,
            queue_depth: self.counters[i].queue_depth.load(Ordering::Relaxed) as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_synctune::VirtualClock;

    fn bus_vt(burst: u32) -> (Arc<VirtualClock>, TelemetryBus<VirtualClock>) {
        let clk = Arc::new(VirtualClock::new());
        let bus = TelemetryBus::new(clk.clone(), burst, 48);
        (clk, bus)
    }

    fn feed_all(bus: &TelemetryBus<VirtualClock>, n: u64) {
        for s in [Stage::Fetch, Stage::Verify, Stage::Commit, Stage::Ingest] {
            bus.on_blocks(s, n);
        }
    }

    #[test]
    fn padded_counters_are_cache_line_aligned() {
        assert!(
            std::mem::align_of::<Padded<StageCounters>>() >= 64,
            "stage counters must be cache-line padded to avoid false sharing"
        );
    }

    #[test]
    fn tick_is_idempotent_without_time() {
        let (_clk, bus) = bus_vt(1000);
        let (k0, _) = bus.tick(); // elapsed 0 -> no step
        assert_eq!(k0, KnobSet::baseline());
        assert_eq!(bus.admit_rate(), TARGET_BLK_S);
    }

    #[test]
    fn governor_throttles_under_congestion_then_recovers() {
        let (clk, bus) = bus_vt(1000);

        // Sustained congestion: bottleneck stage far below target, a queue backing up, stalls.
        for _ in 0..6 {
            bus.on_blocks(Stage::Commit, 5_000); // 5k blk/s << 100k target
            bus.set_queue_depth(Stage::Commit, 8_000);
            bus.on_stall(Stage::Commit);
            clk.advance_ms(1_000);
            bus.tick();
        }
        let throttled = bus.admit_rate();
        assert!(
            throttled < TARGET_BLK_S,
            "admission rate must drop under congestion, got {throttled}"
        );

        // Recovery: all stages healthy above target, queue drained, no new stalls.
        bus.set_queue_depth(Stage::Commit, 0);
        for _ in 0..40 {
            feed_all(&bus, 120_000);
            clk.advance_ms(1_000);
            bus.tick();
        }
        assert_eq!(
            bus.admit_rate(),
            TARGET_BLK_S,
            "admission rate must climb back to the target ceiling once healthy"
        );
    }

    #[test]
    fn structural_knobs_grow_when_sustaining_target() {
        let (clk, bus) = bus_vt(1000);
        let start_window = KnobSet::baseline().window_depth;
        let mut last = start_window;
        for _ in 0..8 {
            feed_all(&bus, 130_000); // healthy & above target across all stages
            clk.advance_ms(1_000);
            let (k, _) = bus.tick();
            last = k.window_depth;
        }
        assert!(
            last > start_window,
            "window should grow while sustaining target ({start_window} -> {last})"
        );
    }

    #[test]
    fn no_structural_growth_during_artificial_relief() {
        let (clk, bus) = bus_vt(1000);
        // Drive real congestion so the governor throttles below target and knobs shrink.
        let mut window_after_congestion = KnobSet::baseline().window_depth;
        for _ in 0..5 {
            bus.on_blocks(Stage::Commit, 5_000);
            bus.set_queue_depth(Stage::Commit, 8_000);
            bus.on_stall(Stage::Commit);
            clk.advance_ms(1_000);
            let (k, _) = bus.tick();
            window_after_congestion = k.window_depth;
        }
        assert!(bus.admit_rate() < TARGET_BLK_S, "precondition: governor throttled");

        // One HEALTHY tick while the rate ceiling is still below target = artificial relief.
        // The guard must HOLD: window must not grow (no capacity added into pending overshoot).
        bus.set_queue_depth(Stage::Commit, 0);
        feed_all(&bus, 130_000);
        clk.advance_ms(1_000);
        let (k, _) = bus.tick();
        assert!(
            bus.admit_rate() < TARGET_BLK_S,
            "rate should still be recovering (below target) on this tick"
        );
        assert!(
            k.window_depth <= window_after_congestion,
            "window must NOT grow during artificial relief ({window_after_congestion} -> {})",
            k.window_depth
        );
    }

    #[test]
    fn coordinated_omission_p99_marks_congested() {
        let (clk, bus) = bus_vt(1000);
        // Throughput looks great, but service is stalling: intended t=0, started 300ms late.
        for _ in 0..4 {
            feed_all(&bus, 120_000);
            bus.record_service(Stage::Commit, 0, 300_000_000, 5_000_000); // ~305ms CO latency
            clk.advance_ms(1_000);
            bus.tick();
        }
        // p99 over the 250ms budget must throttle the spine despite healthy blk/s.
        assert!(
            bus.admit_rate() < TARGET_BLK_S,
            "high p99 under load must congest even with high throughput"
        );
    }

    #[test]
    fn gate_and_spine_share_one_bucket() {
        let (_clk, bus) = bus_vt(500);
        // The dyn gate (serve/fetch admit) and the concrete spine (ingest record) are the
        // same bucket: draining via the gate is visible through the concrete handle.
        let gate = bus.gate();
        let spine = bus.spine();
        assert!(gate.admit(500));
        assert!(!spine.try_acquire(1), "one bucket: gate drain must be seen by spine handle");
    }

    #[test]
    fn stats_report_real_measured_blk_s() {
        let (clk, bus) = bus_vt(1000);
        bus.on_blocks(Stage::Fetch, 90_000);
        clk.advance_ms(1_000); // exactly 1s window
        let (_, stats) = bus.tick();
        let fetch = stats[Stage::Fetch.idx()];
        assert!(
            (fetch.blk_per_sec - 90_000.0).abs() < 1.0,
            "fetch blk/s should be the real measured ~90000, got {}",
            fetch.blk_per_sec
        );
    }
}
