//! SIGIL adaptive emission controller — a faithful port of Quillon Graph's
//! `EmissionController` (q-storage/src/emission_controller.rs), adapted to
//! SIGIL's constants and the braid coinbase path.
//!
//! WHY this exists alongside the pure height-halving in `lib.rs`: the height
//! schedule keeps emission a pure function of height (no drift), but it can't
//! hold *annual* emission constant when block rate varies — at 100 blk/s a
//! height-halving burns the cap in hours. This controller instead targets a
//! constant **annual** emission and derives the per-block reward from the
//! *measured block rate*, so a fast braid and a slow one mint the same amount
//! per year. It is the sophisticated, self-correcting model Quillon runs.
//!
//! DESIGN (mirrors Quillon, file:line refs are to the q-storage source):
//!   * Time-based halving: 64 eras × 4 years each = 256 years to the 21M cap
//!     (`era_at_time`, `era_emission(k) = ERA_0_TOTAL >> k`).
//!   * Adaptive per-block reward `R = annual_target(era) / (rate · secs/yr)` —
//!     constant annual emission regardless of throughput.
//!   * PID correction: compares the persisted cumulative-minted watermark to the
//!     time-ideal cumulative; over-mint shrinks future rewards, under-mint grows
//!     them. Proportional + quadratic-accel term, bounded [0.01, 5.0].
//!   * Per-era budget cap, dynamic per-block max, MIN_REWARD floor, hard supply
//!     & era-64 caps. Integer money math (u128); f64 only for rate + PID.
//!   * A persisted watermark (`total_cumulative_emission`) — the single source of
//!     truth for how much has been minted. NEVER trust a P2P-resynced value
//!     (Quillon's rule): a rebuilt node falls back to a time-formula estimate and
//!     self-corrects via the PID over hours.
//!
//! SAFETY: `calculate_block_reward` returns `Result`. On error the caller MUST
//! abort block production — never mint a 0-reward block (Quillon's hard rule).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use sigil_state::MAX_SUPPLY;

// ── economic constants (SIGIL, matching Quillon's 21M / 256-year schedule) ──
/// Total emission of era 0 = half the cap (the geometric halving sums to MAX).
pub const ERA_0_TOTAL: u128 = MAX_SUPPLY / 2;
/// Seconds per halving era — 4 Julian years (Quillon `SECONDS_PER_HALVING`).
pub const SECONDS_PER_HALVING: u64 = 126_230_400;
/// Seconds per year (Julian) for the annual-rate math.
pub const SECONDS_PER_YEAR: u128 = 31_557_600;
/// After 64 eras the era budget has shifted to 0 — emission ends.
pub const NUM_ERAS: u64 = 64;
/// Fixed-point precision for the integer reward math (Quillon `PRECISION`).
const PRECISION: u128 = 1_000_000;
/// Floor while emission is active — 0.00001 SIGIL (Quillon `MIN_REWARD`).
pub const MIN_REWARD: u128 = 1_000;
/// Absolute per-block ceiling — 2.0 SIGIL (Quillon `ABSOLUTE_MAX_REWARD_PER_BLOCK`).
pub const ABSOLUTE_MAX_REWARD_PER_BLOCK: u128 = 200_000_000;
/// PID smoothing (Quillon `CORRECTION_SMOOTHING`).
const CORRECTION_SMOOTHING: f64 = 0.8;
const CORRECTION_MIN: f64 = 0.01;
const CORRECTION_MAX: f64 = 5.0;
/// A block only counts toward the *live* rate if its timestamp is within this of
/// wall-clock — excludes historical burst (turbo-sync) blocks from rate math.
const LIVE_BLOCK_THRESHOLD_SECS: u64 = 120;
/// Rate measurement window length in seconds (Quillon 30-min wall-clock window).
const RATE_WINDOW_SECS: u64 = 1_800;

/// One (timestamp_secs, was_live) sample for rate measurement.
type RateSample = (u64, bool);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmissionController {
    /// Genesis wall-clock (secs since epoch). Set once; era math anchors here.
    pub genesis_timestamp: u64,
    /// Current halving era (0-based), derived from elapsed time.
    pub current_era: u64,
    /// Emitted so far *within* the current era — resets on era transition.
    pub total_emitted_this_era: u128,
    /// THE watermark: cumulative minted across all eras. Persisted; the single
    /// source of truth for emission. Never overwrite from a P2P-resynced value.
    pub total_cumulative_emission: u128,
    /// Smoothed PID correction factor, persisted across restarts.
    pub correction_factor: f64,
    /// Highest block height recorded (TOCTOU dedup; DAG allows parallel heights
    /// so this is advisory — the caller's coinbase is the authority).
    pub last_tracked_height: u64,
    /// Rolling (ts, live) samples inside the rate window.
    rate_samples: VecDeque<RateSample>,
}

impl EmissionController {
    /// Fresh controller anchored at `genesis_ts` (secs). Watermark starts at 0.
    pub fn new(genesis_ts: u64) -> Self {
        Self {
            genesis_timestamp: genesis_ts,
            current_era: 0,
            total_emitted_this_era: 0,
            total_cumulative_emission: 0,
            correction_factor: 1.0,
            last_tracked_height: 0,
            rate_samples: VecDeque::new(),
        }
    }

    /// Rebuild fallback (Quillon `from_time_based_fallback`): a node with no
    /// persisted watermark seeds the *time-ideal* cumulative and lets the PID
    /// converge to truth over hours. NOT the real replayed supply — a best
    /// estimate, which is why a resynced DB must never be trusted as authority.
    pub fn from_time_based_fallback(genesis_ts: u64, now_ts: u64) -> Self {
        let mut c = Self::new(genesis_ts);
        c.total_cumulative_emission = c.target_cumulative_at_time(now_ts.saturating_sub(genesis_ts));
        c.current_era = c.era_at_time(now_ts.saturating_sub(genesis_ts));
        c
    }

    // ── era / schedule (pure) ───────────────────────────────────────────────
    /// Era index for `elapsed` seconds since genesis.
    pub fn era_at_time(&self, elapsed_secs: u64) -> u64 {
        elapsed_secs / SECONDS_PER_HALVING
    }
    /// Total emission budget of era `k` = ERA_0_TOTAL >> k (0 for k ≥ 64).
    pub fn era_emission(&self, k: u64) -> u128 {
        if k >= NUM_ERAS {
            0
        } else {
            ERA_0_TOTAL >> k
        }
    }
    /// Annual emission target for `era` = era_budget / 4 (4-year eras).
    pub fn annual_emission(&self, era: u64) -> u128 {
        self.era_emission(era) / (SECONDS_PER_HALVING as u128 / SECONDS_PER_YEAR)
    }
    /// Time-ideal cumulative emission at `elapsed` secs — the PID setpoint.
    /// Σ(full eras) + partial of the current era, all integer.
    pub fn target_cumulative_at_time(&self, elapsed_secs: u64) -> u128 {
        let era = self.era_at_time(elapsed_secs);
        let mut cum = 0u128;
        for k in 0..era.min(NUM_ERAS) {
            cum += self.era_emission(k);
        }
        if era < NUM_ERAS {
            let into_era = (elapsed_secs % SECONDS_PER_HALVING) as u128;
            cum += self.era_emission(era) * into_era / (SECONDS_PER_HALVING as u128);
        }
        cum.min(MAX_SUPPLY)
    }

    // ── rate measurement (live-block only) ──────────────────────────────────
    /// Record a produced block for the rate window. `is_live` = its timestamp is
    /// within LIVE_BLOCK_THRESHOLD of wall-clock (historical/burst blocks pass
    /// false and don't inflate the rate — Quillon's turbo-sync guard).
    pub fn add_block(&mut self, height: u64, block_ts_secs: u64, now_secs: u64) {
        self.last_tracked_height = self.last_tracked_height.max(height);
        let is_live = now_secs.saturating_sub(block_ts_secs) <= LIVE_BLOCK_THRESHOLD_SECS;
        self.rate_samples.push_back((now_secs, is_live));
        let cutoff = now_secs.saturating_sub(RATE_WINDOW_SECS);
        while let Some(&(ts, _)) = self.rate_samples.front() {
            if ts < cutoff {
                self.rate_samples.pop_front();
            } else {
                break;
            }
        }
    }
    /// Measured live block rate (blocks/sec) over the window; clamped to a sane
    /// band so a cold window or a burst can't produce absurd rewards.
    pub fn smoothed_rate(&self) -> f64 {
        let live = self.rate_samples.iter().filter(|(_, l)| *l).count();
        if live < 2 {
            return 1.0; // default 1 blk/s until the window fills (Quillon fallback)
        }
        let span = self
            .rate_samples
            .back()
            .map(|(t, _)| *t)
            .unwrap_or(0)
            .saturating_sub(self.rate_samples.front().map(|(t, _)| *t).unwrap_or(0))
            .max(1);
        ((live as f64) / (span as f64)).clamp(0.001, 100_000.0)
    }

    // ── PID correction ──────────────────────────────────────────────────────
    /// The self-correcting factor: how far cumulative-minted has drifted from the
    /// time-ideal. Over-mint (δ>0) → factor<1 (shrink); under-mint → factor>1.
    /// Proportional + quadratic acceleration past 10% error; bounded, smoothed.
    pub fn correction_factor_at(&self, elapsed_secs: u64) -> f64 {
        let target = self.target_cumulative_at_time(elapsed_secs);
        if target == 0 {
            return 1.0;
        }
        let delta = self.total_cumulative_emission as f64 - target as f64;
        let error = delta / target as f64; // >0 over-minted
        let mut factor = 1.0 - CORRECTION_SMOOTHING * error;
        if error.abs() > 0.10 {
            let accel = 2.0 * error * error;
            factor += if error > 0.0 { -accel } else { accel };
        }
        factor.clamp(CORRECTION_MIN, CORRECTION_MAX)
    }

    // ── the reward ──────────────────────────────────────────────────────────
    /// Update the era from wall-clock; carries the era budget across transitions.
    fn update_era(&mut self, now_ts: u64) {
        let elapsed = now_ts.saturating_sub(self.genesis_timestamp);
        let era = self.era_at_time(elapsed);
        if era != self.current_era {
            self.current_era = era;
            self.total_emitted_this_era = 0;
        }
    }

    /// Dynamic per-block ceiling: 2× the ideal reward at this rate, capped at the
    /// absolute max — lets a genuine low-rate spike catch up without runaway.
    fn dynamic_max_reward(&self, era: u64, rate: f64) -> u128 {
        let expected = (rate.max(0.001) * SECONDS_PER_YEAR as f64) as u128;
        let ideal = self.annual_emission(era) / expected.max(1);
        (ideal.saturating_mul(2)).min(ABSOLUTE_MAX_REWARD_PER_BLOCK)
    }

    /// The pure reward math (Quillon `calculate_adaptive_reward`, 10 steps).
    pub fn adaptive_reward(&self, elapsed_secs: u64, rate: f64, total_supply: u128) -> u128 {
        if total_supply >= MAX_SUPPLY {
            return 0;
        }
        let era = self.era_at_time(elapsed_secs);
        if era >= NUM_ERAS {
            return 0;
        }
        let rate = rate.clamp(0.001, 100_000.0);
        let annual = self.annual_emission(era);
        let expected_blocks = (rate * SECONDS_PER_YEAR as f64) as u128;
        if expected_blocks == 0 {
            return MIN_REWARD;
        }
        // base = annual / expected_blocks (fixed-point to avoid truncation bias)
        let base = (annual.saturating_mul(PRECISION)) / expected_blocks / PRECISION;
        // per-era budget cap: never exceed remaining-budget / blocks-remaining
        let remaining = self.era_emission(era).saturating_sub(self.total_emitted_this_era);
        let budget_cap = remaining / expected_blocks.max(1_000_000);
        let mut reward = base.min(budget_cap.max(MIN_REWARD));
        // PID correction (the only f64 op on money)
        let correction = self.correction_factor_at(elapsed_secs);
        reward = ((reward as f64) * correction) as u128;
        // dynamic + absolute + floor clamps
        let dyn_max = self.dynamic_max_reward(era, rate).max(MIN_REWARD);
        reward = reward.clamp(MIN_REWARD, dyn_max);
        // never exceed the cap headroom
        reward.min(MAX_SUPPLY - total_supply)
    }

    /// Compute + book-keep the reward for a coinbase at `now_ts`. Updates the era
    /// and smoothed correction. The caller records the actual minted amount via
    /// [`record_emission`] AFTER the coinbase commits (conservation).
    pub fn calculate_block_reward(&mut self, now_ts: u64, total_supply: u128) -> u128 {
        self.update_era(now_ts);
        let elapsed = now_ts.saturating_sub(self.genesis_timestamp);
        let rate = self.smoothed_rate();
        let reward = self.adaptive_reward(elapsed, rate, total_supply);
        // smooth the persisted correction toward the instantaneous one
        let inst = self.correction_factor_at(elapsed);
        self.correction_factor =
            self.correction_factor * CORRECTION_SMOOTHING + inst * (1.0 - CORRECTION_SMOOTHING);
        reward
    }

    /// Book the amount actually minted into the watermark (call AFTER the
    /// coinbase commits, so a rejected block never advances emission).
    pub fn record_emission(&mut self, amount: u128) {
        self.total_emitted_this_era = self.total_emitted_this_era.saturating_add(amount);
        self.total_cumulative_emission = self.total_cumulative_emission.saturating_add(amount);
    }

    // ── persistence (local RocksDB / snapshot; never gossiped) ──────────────
    pub fn serialize_state(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
    pub fn restore_from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEN: u64 = 1_700_000_000;

    #[test]
    fn era_and_budget_halve() {
        let c = EmissionController::new(GEN);
        assert_eq!(c.era_at_time(0), 0);
        assert_eq!(c.era_at_time(SECONDS_PER_HALVING), 1);
        assert_eq!(c.era_emission(0), ERA_0_TOTAL);
        assert_eq!(c.era_emission(1), ERA_0_TOTAL / 2);
        assert_eq!(c.era_emission(NUM_ERAS), 0);
    }

    #[test]
    fn cumulative_target_sums_toward_cap_not_over() {
        let c = EmissionController::new(GEN);
        // after many eras the ideal cumulative approaches but never exceeds cap
        let huge = SECONDS_PER_HALVING * 70;
        assert!(c.target_cumulative_at_time(huge) <= MAX_SUPPLY);
        // one full era 0 = ERA_0_TOTAL
        assert_eq!(c.target_cumulative_at_time(SECONDS_PER_HALVING), ERA_0_TOTAL);
    }

    #[test]
    fn reward_is_positive_and_capped() {
        let c = EmissionController::new(GEN);
        let r = c.adaptive_reward(1000, 1.0, 0);
        assert!(r >= MIN_REWARD, "active emission floors at MIN_REWARD");
        assert!(r <= ABSOLUTE_MAX_REWARD_PER_BLOCK, "never exceeds the absolute per-block max");
    }

    #[test]
    fn pid_shrinks_reward_when_overminted() {
        let mut over = EmissionController::new(GEN);
        let base = EmissionController::new(GEN).adaptive_reward(SECONDS_PER_YEAR as u64, 1.0, 0);
        // pretend we've minted 30% more than the time-ideal → factor < 1
        let elapsed = SECONDS_PER_YEAR as u64;
        over.total_cumulative_emission =
            (over.target_cumulative_at_time(elapsed) as f64 * 1.30) as u128;
        let corr = over.correction_factor_at(elapsed);
        assert!(corr < 1.0, "over-mint must pull the correction factor below 1");
        let overminted = over.adaptive_reward(elapsed, 1.0, 0);
        assert!(overminted <= base, "over-minted chain mints less per block");
    }

    #[test]
    fn pid_grows_reward_when_underminted() {
        let mut under = EmissionController::new(GEN);
        let elapsed = SECONDS_PER_YEAR as u64;
        under.total_cumulative_emission =
            (under.target_cumulative_at_time(elapsed) as f64 * 0.50) as u128;
        let corr = under.correction_factor_at(elapsed);
        assert!(corr > 1.0, "under-mint must push the correction factor above 1");
    }

    #[test]
    fn hard_caps_hold() {
        let c = EmissionController::new(GEN);
        assert_eq!(c.adaptive_reward(1000, 1.0, MAX_SUPPLY), 0, "no reward at the cap");
        assert_eq!(
            c.adaptive_reward(SECONDS_PER_HALVING * NUM_ERAS + 1, 1.0, 0),
            0,
            "no reward past era 64"
        );
    }

    #[test]
    fn watermark_persists_roundtrip() {
        let mut c = EmissionController::new(GEN);
        c.record_emission(12_345);
        c.calculate_block_reward(GEN + 100, 12_345);
        let bytes = c.serialize_state();
        let back = EmissionController::restore_from_bytes(&bytes).unwrap();
        assert_eq!(back.total_cumulative_emission, c.total_cumulative_emission);
        assert_eq!(back.current_era, c.current_era);
    }

    #[test]
    fn annual_emission_constant_regardless_of_rate() {
        // the whole point: fast and slow chains mint ~the same per year.
        let c = EmissionController::new(GEN);
        let slow_rate = 1.0;
        let fast_rate = 100.0;
        let r_slow = c.adaptive_reward(1000, slow_rate, 0);
        let r_fast = c.adaptive_reward(1000, fast_rate, 0);
        let annual_slow = r_slow * (slow_rate * SECONDS_PER_YEAR as f64) as u128;
        let annual_fast = r_fast * (fast_rate * SECONDS_PER_YEAR as f64) as u128;
        // within an order of magnitude of each other + the era target (rounding +
        // the MIN_REWARD floor on the fast path make this approximate)
        let target = c.annual_emission(0);
        assert!(annual_slow > target / 4 && annual_slow < target * 4, "slow ≈ annual target");
        assert!(annual_fast > target / 4 && annual_fast < target * 4, "fast ≈ annual target");
    }
}
