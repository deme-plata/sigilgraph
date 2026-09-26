//! Adaptive block-production rate governor — demand-responsive, calibrated.
//!
//! # Model
//! Block production is a service that drains the mempool (a transaction queue).
//! Let `backlog` = verified txs awaiting inclusion and `cap` = txs packed per
//! block. By **Little's Law** the mean inclusion latency is `L = backlog / μ`,
//! where the service rate `μ = rate · cap` (txs/s). To hold `L ≤ L*` (the target
//! latency) the block rate must satisfy `rate ≥ backlog / (L* · cap)`. So the
//! governor's instantaneous target is
//!
//! ```text
//!     rate_target = clamp( backlog / (L* · cap),  rate_min,  rate_max )
//! ```
//!
//! An idle chain (`backlog = 0`) settles at `rate_min` — a liveness heartbeat,
//! no wasted empty blocks. A loaded chain speeds up *only as far as needed* to
//! keep tx latency bounded, and never past `rate_max` (the sync/network ceiling).
//!
//! # Stability
//! `backlog` is bursty, so the raw target is low-pass filtered with an
//! **asymmetric EMA**: a fast attack (`alpha_up`) absorbs a load spike within a
//! couple of control ticks; a slow decay (`alpha_down`) stops the rate flapping
//! back down on a transient lull. This is a first-order filter with hysteresis —
//! the same shape as TCP/AIMD and EIP-1559's fee controller — and for
//! `alpha ∈ (0,1]` it converges monotonically without oscillation.
//!
//! # Calibration (all env-tunable)
//! - `SIGIL_RATE_MIN`  (blk/s, default 8)   idle heartbeat floor
//! - `SIGIL_RATE_MAX`  (blk/s, default 60)  ceiling (sync/network capacity)
//! - `SIGIL_RATE_TARGET_LATENCY_MS` (default 2000)  max tx inclusion latency L*
//! - `SIGIL_RATE_TX_CAPACITY` (default = txgen, else 256)  drain quantum cap
//! - `SIGIL_RATE_ATTACK` (0..1, default 0.5)  EMA gain speeding up
//! - `SIGIL_RATE_DECAY`  (0..1, default 0.1)  EMA gain slowing down
//! - `SIGIL_RATE_TARGET_FINALITY_MS` (unset = OFF)  settlement-latency target F*
//!
//! # Settlement latency, not only inclusion latency (2026-09-26)
//! Little's Law above bounds *inclusion*. But a payment is only final once the
//! braid's depth rule has buried it `final_depth` blocks deep, so its settlement
//! time is `final_depth / rate`. Measured on the live producer 2026-09-26: rate
//! 7.98 blk/s, `final_depth` 512, so every payment waited **64 s** to settle while
//! its own inclusion took 125 ms. One payment adds a backlog of 1, which the
//! inclusion term turns into 0.002 blk/s of demand, so the rate never moved.
//!
//! With `F*` set, any NEW in-flight transaction arms a *settlement boost*: the
//! target becomes `final_depth / F*` (clamped to the ceiling) for a bounded budget
//! of `final_depth · 5/4` blocks, entered at once rather than through the EMA.
//! The budget is what keeps a stuck transaction (one the node keeps refusing)
//! from pinning the chain at the ceiling forever — the 10.5 GB/day of empty
//! blocks measured on 2026-09-13. The depth rule itself is untouched: the same
//! 512 blocks bury the payment, they just arrive faster. Producer-local; no
//! follower validates the block rate, so this is not a consensus change.

use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RateGovernor {
    pub rate_min: f64,
    pub rate_max: f64,
    pub target_latency_s: f64,
    pub tx_capacity: f64,
    pub alpha_up: f64,
    pub alpha_down: f64,
    rate: f64,
    /// Settlement boost (None = off, the pre-2026-09-26 behaviour exactly).
    pub settle: Option<SettleBoost>,
}

/// Config + state of the settlement-latency term. See the module doc.
#[derive(Clone, Debug)]
pub struct SettleBoost {
    /// blk/s that buries a payment `final_depth` deep within F*, clamped to the ceiling.
    pub rate: f64,
    /// Blocks of boost granted per newly observed in-flight transaction.
    pub budget_blocks: u64,
    left: u64,
    last_inflight: usize,
}

impl SettleBoost {
    pub fn new(final_depth: u64, target_finality_s: f64, rate_min: f64, rate_max: f64) -> Self {
        let depth = final_depth.max(1) as f64;
        SettleBoost {
            rate: (depth / target_finality_s.max(0.05)).clamp(rate_min, rate_max),
            budget_blocks: final_depth.max(1) + final_depth / 4,
            left: 0,
            last_inflight: 0,
        }
    }
    pub fn active(&self) -> bool { self.left > 0 }
    pub fn blocks_left(&self) -> u64 { self.left }
}

impl RateGovernor {
    /// Build from env. `tx_capacity_default` is the producer's per-block tx cap
    /// (SIGIL_TXGEN when set) used when `SIGIL_RATE_TX_CAPACITY` is absent.
    pub fn from_env(tx_capacity_default: f64) -> Self {
        let f = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(d);
        let rate_min = f("SIGIL_RATE_MIN", 8.0).max(0.1);
        let rate_max = f("SIGIL_RATE_MAX", 60.0).max(rate_min);
        RateGovernor {
            rate_min,
            rate_max,
            target_latency_s: (f("SIGIL_RATE_TARGET_LATENCY_MS", 2000.0) / 1000.0).max(0.05),
            tx_capacity: f("SIGIL_RATE_TX_CAPACITY", tx_capacity_default).max(1.0),
            alpha_up: f("SIGIL_RATE_ATTACK", 0.5).clamp(0.01, 1.0),
            alpha_down: f("SIGIL_RATE_DECAY", 0.1).clamp(0.01, 1.0),
            rate: rate_min, // start at the floor — "begin with few blocks"
            settle: None,
        }
    }

    /// Arm the settlement term. `target_finality_ms` = None leaves it OFF.
    pub fn with_settlement(mut self, final_depth: u64, target_finality_ms: Option<f64>) -> Self {
        self.settle = target_finality_ms
            .filter(|ms| *ms > 0.0)
            .map(|ms| SettleBoost::new(final_depth, ms / 1000.0, self.rate_min, self.rate_max));
        self
    }

    /// Call once per PRODUCED block with the number of transactions still in flight
    /// (submitted, not yet settled). Returns the new inter-block interval when the rate
    /// had to change at once — the caller re-arms its tick; `None` = keep the tick.
    ///
    /// A rise in `inflight` means a new payment arrived: grant a fresh budget and jump
    /// straight to the boost rate (no EMA — the EMA's first step would only come after
    /// the 16-block cadence, which at the 8 blk/s floor alone is 2 s of a 5 s target).
    pub fn on_block(&mut self, inflight: usize) -> Option<Duration> {
        let floor = self.rate_min;
        let b = self.settle.as_mut()?;
        let arrived = inflight > b.last_inflight;
        b.last_inflight = inflight;
        let was = b.left > 0;
        if arrived {
            b.left = b.budget_blocks;
        } else if b.left > 0 {
            b.left -= 1;
        }
        if inflight == 0 {
            b.left = 0; // everything settled: nothing left to hurry
        }
        let now = b.left > 0;
        let boost = b.rate;
        if now && self.rate < boost {
            self.rate = boost;
            return Some(Self::interval(self.rate));
        }
        if was && !now {
            // Boost over: fall back to the floor at once rather than decaying through
            // ~40 EMA steps of mostly-empty blocks. The next `update` re-applies any
            // genuine inclusion backlog.
            self.rate = floor;
            return Some(Self::interval(self.rate));
        }
        None
    }

    fn interval(rate: f64) -> Duration {
        Duration::from_micros(((1_000_000.0 / rate).round() as u64).max(50))
    }

    /// Feed the current mempool backlog; returns the next inter-block interval.
    /// Pure + deterministic (no clock read) so it is unit-testable and replayable.
    pub fn update(&mut self, backlog: usize) -> Duration {
        let demand = backlog as f64 / (self.target_latency_s * self.tx_capacity);
        let settle = self.settle.as_ref().filter(|b| b.active()).map(|b| b.rate).unwrap_or(0.0);
        let target = demand.max(settle).clamp(self.rate_min, self.rate_max);
        let alpha = if target >= self.rate { self.alpha_up } else { self.alpha_down };
        self.rate = (self.rate + alpha * (target - self.rate)).clamp(self.rate_min, self.rate_max);
        let us = (1_000_000.0 / self.rate).round() as u64;
        Duration::from_micros(us.max(50))
    }

    pub fn rate(&self) -> f64 { self.rate }
    pub fn settle_active(&self) -> bool { self.settle.as_ref().map(|b| b.active()).unwrap_or(false) }
    /// blk/s the governor would run at RIGHT NOW for a given backlog, without
    /// mutating state — for the TUI/feed readout.
    pub fn peek_target(&self, backlog: usize) -> f64 {
        (backlog as f64 / (self.target_latency_s * self.tx_capacity)).clamp(self.rate_min, self.rate_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gov() -> RateGovernor {
        RateGovernor { rate_min: 8.0, rate_max: 60.0, target_latency_s: 2.0,
            tx_capacity: 256.0, alpha_up: 0.5, alpha_down: 0.1, rate: 8.0, settle: None }
    }
    /// The live producer's numbers: floor 8, ceiling 110, depth 512, F* = 5 s.
    fn live() -> RateGovernor {
        RateGovernor { rate_min: 8.0, rate_max: 110.0, target_latency_s: 2.0,
            tx_capacity: 256.0, alpha_up: 0.5, alpha_down: 0.1, rate: 8.0, settle: None }
            .with_settlement(512, Some(5000.0))
    }
    /// Simulate block production: returns (seconds until `blocks` blocks have been
    /// produced after a payment arrives, blocks produced at the boost rate).
    fn settle_time(g: &mut RateGovernor, blocks: u64) -> f64 {
        let mut t = 0.0;
        let mut inflight = 1usize;
        let mut n = 0u64;
        while n < blocks {
            if let Some(_) = g.on_block(inflight) {}
            t += 1.0 / g.rate();
            n += 1;
            if n % 16 == 0 { g.update(inflight); }
            if n == blocks { inflight = 0; }
        }
        t
    }
    #[test]
    fn off_by_default_changes_nothing() {
        let mut a = gov();
        let mut b = gov().with_settlement(512, None);
        for i in 0..200 { assert_eq!(a.update(i % 7), b.update(i % 7)); assert!(b.on_block(3).is_none()); }
    }
    #[test]
    fn one_payment_settles_in_about_f_star_not_64_seconds() {
        let mut g = live();
        let t = settle_time(&mut g, 512);
        // measured baseline: 512 blocks at the 8 blk/s floor = 64 s
        assert!(t < 5.5, "one payment took {t:.2} s to be buried 512 deep (F* = 5 s, ceiling 110 = 4.65 s)");
        assert!(t > 4.0, "{t:.2} s is faster than the ceiling allows — the model is wrong");
        let mut off = gov(); off.rate_max = 110.0;
        let t_off = settle_time(&mut off, 512);
        assert!((t_off - 64.0).abs() < 0.01, "baseline must reproduce the measured 64 s, got {t_off:.2}");
    }
    #[test]
    fn a_stuck_payment_cannot_pin_the_ceiling() {
        let mut g = live();
        // inflight never drops (a tx the node keeps refusing): boost must end by budget
        let mut boosted = 0u64;
        for n in 0..10_000u64 {
            g.on_block(1);
            if g.settle_active() { boosted += 1; }
            if n % 16 == 0 { g.update(1); }
        }
        assert_eq!(boosted, 512 + 128, "boost must last exactly the 640-block budget, then stop");
        assert!((g.rate() - 8.0).abs() < 1e-9, "after the budget the chain must be back at the floor");
    }
    #[test]
    fn a_new_payment_rearms_the_budget_and_settling_ends_it() {
        let mut g = live();
        assert!(g.on_block(1).is_some(), "first payment must re-arm the tick at once");
        assert!((g.rate() - 102.4).abs() < 1e-9, "512 blocks / 5 s = 102.4 blk/s");
        for _ in 0..300 { g.on_block(1); }
        let left = g.settle.as_ref().unwrap().blocks_left();
        g.on_block(2); // a second payment arrives mid-boost
        assert!(g.settle.as_ref().unwrap().blocks_left() > left, "a new arrival must grant a fresh budget");
        assert!(g.on_block(0).is_some(), "all settled: drop to the floor at once");
        assert!((g.rate() - 8.0).abs() < 1e-9);
        assert!(!g.settle_active());
    }
    /// What the boost costs in EMISSION, measured with the REAL controller rather than
    /// argued: 1 h warm-up at the floor, then 6 h with one payment per minute, boost
    /// OFF vs ON. The controller sizes each block's reward from a 30-min smoothed rate
    /// plus a PID term toward the time-ideal curve, so a 5 s burst of ~600 blocks is
    /// paid at the idle-sized reward until the smoothing and the correction catch up.
    /// The existing chronos emission test already tolerates 15 % while the rate swings
    /// 8–60 blk/s; the boost must stay inside that same envelope.
    #[test]
    fn settlement_boost_emission_cost_is_bounded() {
        use sigil_emission::controller::EmissionController;
        const GEN: u64 = 1_786_752_000;
        fn run(boost: bool) -> (u128, u64) {
            let mut c = EmissionController::new(GEN);
            let mut g = live();
            if !boost { g.settle = None; }
            let (warm, end) = (3_600.0, 3_600.0 + 6.0 * 3_600.0);
            let (mut t, mut h, mut supply, mut minted, mut blocks) = (0.0f64, 0u64, 0u128, 0u128, 0u64);
            let (mut next_pay, mut inflight, mut bury) = (warm, 0usize, 0u64);
            while t < end {
                if t >= next_pay { inflight += 1; bury = 512; next_pay += 60.0; }
                g.on_block(inflight);
                h += 1;
                let ts = GEN + t as u64;
                c.add_block(h, ts, ts);
                let r = c.calculate_block_reward(ts, supply);
                c.record_emission(r);
                supply += r;
                if t >= warm { minted += r; blocks += 1; }
                if bury > 0 { bury -= 1; if bury == 0 { inflight = 0; } }
                if h % 16 == 0 { g.update(0); }
                t += 1.0 / g.rate();
            }
            (minted, blocks)
        }
        let (base, nb) = run(false);
        let (boost, bb) = run(true);
        let ratio = boost as f64 / base as f64;
        eprintln!("emission over 6 h, 1 payment/min: OFF {base} raw in {nb} blocks · ON {boost} raw in {bb} blocks · ratio {ratio:.4}");
        assert!(bb > nb, "the boost must actually produce more blocks");
        assert!(ratio < 1.15, "boost over-emits by {:.1} % — outside the 15 % envelope the chronos test allows", (ratio - 1.0) * 100.0);
    }
    #[test]
    fn boost_never_exceeds_the_ceiling() {
        let mut g = live().with_settlement(512, Some(100.0)); // 5,120 blk/s requested
        g.on_block(1);
        assert!((g.rate() - 110.0).abs() < 1e-9);
    }
    fn steady(backlog: usize) -> f64 { let mut g = gov(); for _ in 0..1000 { g.update(backlog); } g.rate() }

    #[test]
    fn idle_settles_at_floor() {
        assert!((steady(0) - 8.0).abs() < 1e-9, "empty mempool must rest at the floor");
    }
    #[test]
    fn huge_backlog_saturates_ceiling() {
        assert!((steady(100_000_000) - 60.0).abs() < 1e-6, "unbounded demand must clamp at ceiling");
    }
    #[test]
    fn rate_is_monotonic_nondecreasing_in_backlog() {
        assert!(steady(0) <= steady(5_000));
        assert!(steady(5_000) <= steady(50_000));
        assert!(steady(50_000) <= steady(5_000_000));
    }
    #[test]
    fn latency_bound_holds_in_the_active_band() {
        // backlog chosen so demand = 20 blk/s (inside [8,60]); L should hit L*.
        let backlog = (20.0 * 2.0 * 256.0) as usize; // rate*L*·cap
        let mut g = gov(); for _ in 0..1000 { g.update(backlog); }
        let latency = backlog as f64 / (g.rate() * 256.0);
        assert!(latency <= 2.0 + 1e-6, "steady-state latency {latency} must be <= L*=2s");
    }
    #[test]
    fn attack_is_faster_than_decay() {
        let mut up = gov(); let a = up.rate(); up.update(100_000_000); let d_up = up.rate() - a;
        let mut dn = gov(); dn.rate = 60.0; let b = dn.rate(); dn.update(0); let d_dn = b - dn.rate();
        assert!(d_up > d_dn, "attack {d_up} must exceed decay {d_dn}");
    }
    #[test]
    fn interval_is_sane_and_bounded() {
        let mut g = gov();
        let idle = g.update(0);            assert_eq!(idle, Duration::from_micros(125_000)); // 8/s
        let mut g2 = gov(); for _ in 0..1000 { g2.update(100_000_000); }
        let hot = g2.update(100_000_000);  assert_eq!(hot, Duration::from_micros(16_667));  // ~60/s
    }
}
