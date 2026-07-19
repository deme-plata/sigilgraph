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
        }
    }

    /// Feed the current mempool backlog; returns the next inter-block interval.
    /// Pure + deterministic (no clock read) so it is unit-testable and replayable.
    pub fn update(&mut self, backlog: usize) -> Duration {
        let demand = backlog as f64 / (self.target_latency_s * self.tx_capacity);
        let target = demand.clamp(self.rate_min, self.rate_max);
        let alpha = if target >= self.rate { self.alpha_up } else { self.alpha_down };
        self.rate = (self.rate + alpha * (target - self.rate)).clamp(self.rate_min, self.rate_max);
        let us = (1_000_000.0 / self.rate).round() as u64;
        Duration::from_micros(us.max(50))
    }

    pub fn rate(&self) -> f64 { self.rate }
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
            tx_capacity: 256.0, alpha_up: 0.5, alpha_down: 0.1, rate: 8.0 }
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
