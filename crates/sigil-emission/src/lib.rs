//! sigil-emission — the SIGIL block-reward schedule.
//!
//! Bitcoin-style **height-based halving**: the reward starts at
//! [`INITIAL_BLOCK_REWARD`] and halves every [`HALVING_INTERVAL`] blocks. The
//! constants are chosen so `INITIAL_BLOCK_REWARD × HALVING_INTERVAL =
//! MAX_SUPPLY / 2` — the geometric sum `Σ R0·H·(½)^e → 2·R0·H = MAX_SUPPLY`,
//! approaching the 21M cap but **never reaching it** (and dropping to 0 once the
//! shift exhausts the reward).
//!
//! WHY this design (the Quillon-postmortem fix): emission here is a PURE
//! function of height — there is no separate, mutable "emission state" to drift
//! or be blindly overwritten (which caused Quillon's 3-day wrong-emission bug).
//! The reward this returns is minted via `submit_share` → `commit_state_transition`,
//! so the only emission record that exists is the committed native supply in
//! `wallet_state_root`, and the chokepoint independently refuses any post-state
//! above `MAX_SUPPLY`. Schedule + cap are two independent guards.

use sigil_state::MAX_SUPPLY;

/// The breathing observer + ±2% band enforcer (Quillon-postmortem fix, kept as a
/// fail-loud verifier rather than a drift-prone actuator). See [`breathing`].
pub mod breathing;

/// Blocks per halving epoch.
pub const HALVING_INTERVAL: u64 = 2_100_000;

/// Reward for the genesis epoch, in base units (8 decimals → 5.00000000 SIGIL).
/// `INITIAL_BLOCK_REWARD × HALVING_INTERVAL == MAX_SUPPLY / 2`.
pub const INITIAL_BLOCK_REWARD: u128 = 500_000_000;

const _: () = assert!(
    INITIAL_BLOCK_REWARD * (HALVING_INTERVAL as u128) == MAX_SUPPLY / 2,
    "emission schedule must sum to exactly the 21M cap"
);

/// After this many halvings the reward has shifted to 0; clamp to avoid a
/// shift-overflow panic and to make emission terminate cleanly.
const MAX_EPOCHS: u32 = 64;

/// The block reward minted to the coinbase at `height`. Deterministic; pass this
/// as the `reward` to `sigil_rpc::submit_share` (or the producer's coinbase).
pub fn block_reward(height: u64) -> u128 {
    let epoch = (height / HALVING_INTERVAL) as u32;
    if epoch >= MAX_EPOCHS {
        return 0;
    }
    INITIAL_BLOCK_REWARD >> epoch
}

/// Total SIGIL emitted across blocks `0..height` (exclusive). Strictly less than
/// [`MAX_SUPPLY`] for every height — the schedule can never breach the cap.
pub fn cumulative_emission(height: u64) -> u128 {
    let full = (height / HALVING_INTERVAL).min(MAX_EPOCHS as u64) as u32;
    let rem = (height % HALVING_INTERVAL) as u128;
    let mut cum: u128 = 0;
    for e in 0..full {
        cum += (INITIAL_BLOCK_REWARD >> e) * (HALVING_INTERVAL as u128);
    }
    if full < MAX_EPOCHS {
        cum += (INITIAL_BLOCK_REWARD >> full) * rem;
    }
    cum
}

// ── Time-based emission (LANE-R, QUG model) ──────────────────────────────────────────
// Block-based halving burns a whole 2.1M-block epoch every ~2.65h at 220 blk/s, so the
// testnet hit the 21M cap in HOURS. TIME-based halving makes emission independent of the
// block rate: the same SIGIL is minted per unit of WALL-CLOCK time no matter how fast
// blocks are produced. The two callers (producer + /api/v1/mining/submit) pass block
// timestamps + the genesis timestamp instead of a height.

/// Halving period: 4 years in seconds (4 × 365 × 86400). Reward halves each period.
pub const HALVING_PERIOD_SECS: u64 = 126_144_000;

/// Microseconds per second — block timestamps are carried in µs so a 400 µs block isn't
/// rounded to a 0 reward.
pub const US_PER_SEC: u128 = 1_000_000;

/// Exact-fraction denominator: a slice's numerator `(MAX_SUPPLY/2 >> epoch) × dt_µs`
/// divided by this yields the base-unit reward. = `HALVING_PERIOD_SECS × 1e6`. We do NOT
/// pre-divide `MAX_SUPPLY/2` by the period (that truncates the per-second rate ~8.3M→lossy);
/// we keep the full numerator and carry the remainder across blocks, so emission lost to
/// integer truncation is ZERO. Σ over all epochs of `(MAX_SUPPLY/2 >> e) = MAX_SUPPLY`, so
/// the curve asymptotes to EXACTLY the 21M cap.
pub const EMISSION_DENOM: u128 = HALVING_PERIOD_SECS as u128 * US_PER_SEC;

/// One full 4-year epoch emits exactly half the remaining-at-that-epoch cap — at epoch 0
/// that is `MAX_SUPPLY/2`. (Documents the time-model invariant the way the block model's
/// `INITIAL_BLOCK_REWARD × HALVING_INTERVAL == MAX_SUPPLY/2` assert does.)
const _: () = assert!(
    cumulative_emission_time_const(HALVING_PERIOD_SECS) == MAX_SUPPLY / 2,
    "the first 4-year epoch must emit exactly half the 21M cap"
);

/// `const fn` mirror of [`cumulative_emission_time`] so the invariant above is enforced at
/// compile time. (A separate const fn because trait-bound float/loop helpers can't be const.)
const fn cumulative_emission_time_const(elapsed_secs: u64) -> u128 {
    let elapsed_us = elapsed_secs as u128 * US_PER_SEC;
    let full = {
        let f = elapsed_secs / HALVING_PERIOD_SECS;
        if f > MAX_EPOCHS as u64 { MAX_EPOCHS } else { f as u32 }
    };
    let mut cum: u128 = 0;
    let mut e: u32 = 0;
    while e < full { cum += (MAX_SUPPLY / 2) >> e; e += 1; }
    if full < MAX_EPOCHS {
        let rem_us = elapsed_us - (full as u128 * EMISSION_DENOM);
        cum += (((MAX_SUPPLY / 2) >> full) * rem_us) / EMISSION_DENOM;
    }
    cum
}

/// The TIME-based block reward: the integral of the emission rate over the wall-clock slice
/// `[prev_ts_us, block_ts_us]` (µs since the unix epoch). The rate at elapsed time
/// `t = ts − genesis_ts` is `(MAX_SUPPLY/2 >> (t / HALVING_PERIOD))` base units per period;
/// an epoch boundary inside the slice is split EXACTLY. `carry_in` is the sub-unit numerator
/// remainder from the previous block — thread the returned remainder into the next call so
/// truncation loss across the chain is zero. Returns `(reward_base_units, remainder_out)`.
pub fn block_reward_time(genesis_ts_us: u128, prev_ts_us: u128, block_ts_us: u128, carry_in: u128) -> (u128, u128) {
    if block_ts_us <= prev_ts_us { return (0, carry_in); }
    let mut numer: u128 = carry_in;          // accumulated numerator (base-units × EMISSION_DENOM)
    let mut a = prev_ts_us.max(genesis_ts_us); // nothing is emitted before genesis
    while a < block_ts_us {
        let elapsed = a - genesis_ts_us;                       // µs since genesis
        let epoch = (elapsed / EMISSION_DENOM) as u32;          // = elapsed_s / HALVING_PERIOD_SECS
        let next_boundary = genesis_ts_us + ((epoch as u128 + 1) * EMISSION_DENOM);
        let b = block_ts_us.min(next_boundary);
        let dt = b - a;                                        // µs in this epoch
        let half = if epoch >= MAX_EPOCHS { 0 } else { (MAX_SUPPLY / 2) >> epoch };
        numer += half * dt;
        a = b;
    }
    (numer / EMISSION_DENOM, numer % EMISSION_DENOM)
}

/// Total emitted from genesis to `elapsed_secs` of wall-clock time — the time-based analogue
/// of [`cumulative_emission`]. Strictly `< MAX_SUPPLY`; `→ MAX_SUPPLY` as `elapsed → ∞`.
pub fn cumulative_emission_time(elapsed_secs: u64) -> u128 {
    cumulative_emission_time_const(elapsed_secs)
}

// ── H7: block-timestamp future-bound guard (height-gated, DORMANT) ──────────────────
// Time-based emission makes the block timestamp a MONEY input: the reward is the
// integral of the emission rate over [prev_ts, block_ts]. The producer stamps its own
// local clock, but a follower applying a peer's block recomputes the reward from the
// peer's STORED ts verbatim — without a bound, a producer stamping future timestamps
// pulls the emission curve forward (capped at 21M, but it steals the schedule). This
// guard rejects any block stamped beyond MAX_FUTURE_DRIFT_US ahead of the verifier's
// clock. Backwards timestamps need no guard here: block_reward_time() already yields
// 0 reward for ts ≤ prev_ts, so the past cannot mint.

/// Maximum tolerated forward stamp, in µs (2 minutes). Bitcoin tolerates 2 hours;
/// SIGIL producers are NTP-synced servers stamping µs, so 2 minutes is generous
/// headroom for real skew while capping what a future-stamper can pull forward to
/// ~2 minutes of emission (~10 SIGIL at epoch 0).
pub const MAX_FUTURE_DRIFT_US: u128 = 120 * US_PER_SEC;

/// Activation height for H7 (future-timestamp bound on block apply).
/// **Dormant by default** — `u64::MAX` means "never active", so merging this code
/// changes nothing on the live chain until an operator schedules a real future
/// height (see `docs/H7_TS_GUARD_GO_NO_GO.md` for the activation prerequisites).
pub const H7_TS_GUARD_ACTIVATION_HEIGHT: u64 = u64::MAX;

/// H7 rejection: a block stamped in the verifier's future beyond the drift bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FutureTimestamp {
    pub height: u64,
    pub block_ts_us: u128,
    pub local_now_us: u128,
    /// How far past the bound: `block_ts_us − (local_now_us + MAX_FUTURE_DRIFT_US)`.
    pub excess_us: u128,
}

impl core::fmt::Display for FutureTimestamp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "block {} stamped {} µs beyond the future bound (ts {} vs local {} + {} drift)",
            self.height, self.excess_us, self.block_ts_us, self.local_now_us, MAX_FUTURE_DRIFT_US
        )
    }
}

impl std::error::Error for FutureTimestamp {}

/// Pure, ungated predicate: is `block_ts_us` within the future bound of the
/// verifier's clock? Exposed separately so telemetry can observe drift before H7
/// is ever activated.
pub fn ts_within_future_bound(block_ts_us: u128, local_now_us: u128) -> bool {
    block_ts_us <= local_now_us + MAX_FUTURE_DRIFT_US
}

/// The height-gated H7 check for the follower APPLY path. Below the activation
/// height it is a no-op (every historical block still applies, exact legacy
/// behaviour); at/above it rejects blocks stamped beyond the future bound.
pub fn check_block_ts(height: u64, block_ts_us: u128, local_now_us: u128) -> Result<(), FutureTimestamp> {
    if height < H7_TS_GUARD_ACTIVATION_HEIGHT {
        return Ok(());
    }
    if ts_within_future_bound(block_ts_us, local_now_us) {
        return Ok(());
    }
    Err(FutureTimestamp {
        height,
        block_ts_us,
        local_now_us,
        excess_us: block_ts_us - (local_now_us + MAX_FUTURE_DRIFT_US),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_and_halvings() {
        assert_eq!(block_reward(0), INITIAL_BLOCK_REWARD);
        assert_eq!(block_reward(HALVING_INTERVAL - 1), INITIAL_BLOCK_REWARD);
        assert_eq!(block_reward(HALVING_INTERVAL), INITIAL_BLOCK_REWARD / 2);
        assert_eq!(block_reward(HALVING_INTERVAL * 10), INITIAL_BLOCK_REWARD >> 10);
        assert_eq!(block_reward(HALVING_INTERVAL * 200), 0, "reward terminates");
    }

    #[test]
    fn never_exceeds_21m_cap() {
        // Across the entire schedule, cumulative emission stays UNDER the cap.
        assert!(cumulative_emission(u64::MAX / 2) <= MAX_SUPPLY);
        assert!(cumulative_emission(HALVING_INTERVAL * 64) < MAX_SUPPLY);
        // ... but it gets very close (geometric series → 21M).
        let near = cumulative_emission(HALVING_INTERVAL * 60);
        assert!(near > MAX_SUPPLY * 9 / 10 && near < MAX_SUPPLY);
    }

    #[test]
    fn monotonic_nondecreasing() {
        assert!(cumulative_emission(1_000) < cumulative_emission(2_000));
        assert!(cumulative_emission(HALVING_INTERVAL) < cumulative_emission(HALVING_INTERVAL * 2));
    }

    #[test]
    fn first_epoch_emits_exactly_half_the_cap() {
        // One full epoch at the initial reward = MAX_SUPPLY / 2.
        assert_eq!(cumulative_emission(HALVING_INTERVAL), MAX_SUPPLY / 2);
    }

    // ── CHRONOS EMISSION GATE (LANE-R, consensus-critical) ───────────────────────────
    // Sum block_reward_time() across a wall-clock window at two very different block rates
    // and assert the cumulative emission is IDENTICAL in TIME — block-rate independent —
    // and equals the closed-form time integral exactly (the remainder carry loses nothing).
    fn run_blocks(genesis: u128, window_secs: u64, blk_per_s: u64) -> u128 {
        let total_us = window_secs as u128 * US_PER_SEC;
        let n = window_secs * blk_per_s;          // blocks in the window
        let mut prev = genesis;
        let mut carry: u128 = 0;
        let mut total: u128 = 0;
        for i in 1..=n {
            // contiguous, exact: block i ends at genesis + total_us·i/n (last lands at +total_us)
            let ts = genesis + (total_us * i as u128) / n as u128;
            let (r, c) = block_reward_time(genesis, prev, ts, carry);
            total += r; carry = c; prev = ts;
        }
        total
    }

    #[test]
    fn chronos_emission_is_block_rate_independent() {
        let genesis = 1_700_000_000_000_000u128; // arbitrary genesis (µs since unix epoch)
        let window = 3600u64;                     // one hour of virtual time
        let fast = run_blocks(genesis, window, 220); // ~220 blk/s producer
        let slow = run_blocks(genesis, window, 2);   // ~2 blk/s
        let exact = cumulative_emission_time(window);
        // (1) identical in TIME regardless of block rate — THE consensus property
        assert_eq!(fast, slow, "emission must be identical in time at 220 vs 2 blk/s");
        // and equal to the closed-form integral (carry preserves every base unit)
        assert_eq!(fast, exact, "block-summed emission must equal the time integral exactly");
        // sanity: an hour at epoch 0 emits ~ (MAX_SUPPLY/2)/period · 3600s of base units
        assert!(fast > 0);
    }

    #[test]
    fn chronos_emission_halves_every_period_and_caps() {
        // (4) reward halves every HALVING_PERIOD_SECS: a full epoch-1 emits half of epoch-0
        let g = 1_700_000_000_000_000u128;
        let epoch0 = block_reward_time(g, g, g + EMISSION_DENOM, 0).0;
        let epoch1 = block_reward_time(g, g + EMISSION_DENOM, g + 2 * EMISSION_DENOM, 0).0;
        assert_eq!(epoch0, MAX_SUPPLY / 2, "first 4-year epoch emits half the cap");
        assert_eq!(epoch1, MAX_SUPPLY / 4, "second epoch emits a quarter — halved");
        // (2)+(3) asymptotic to 21M, never exceeds the cap
        let many_years = HALVING_PERIOD_SECS * 60; // 240 years → 60 halvings
        let cum = cumulative_emission_time(many_years);
        assert!(cum < MAX_SUPPLY, "cumulative emission never exceeds the 21M cap");
        assert!(cum > MAX_SUPPLY * 9 / 10, "approaches the cap asymptotically");
        // two halvings (8y) ≈ 3/4 of the cap (1/2 + 1/4)
        assert_eq!(cumulative_emission_time(HALVING_PERIOD_SECS * 2), MAX_SUPPLY * 3 / 4);
    }

    #[test]
    fn produce_verify_equivalence() {
        // The consensus invariant: the PRODUCER stamps a block ts + computes block_reward_time
        // (genesis, its last_block_ts, ts, 0); a FOLLOWER recomputes from the SAME stored ts +
        // ITS own independently-tracked last_block_ts. They MUST agree block-by-block (any
        // divergence = a chain fork) and the chain total MUST equal the closed-form integral.
        let genesis = 1_700_000_000_000_000u128;
        let mut prod_prev = genesis; let mut prod_carry = 0u128; // producer state
        let mut ver_prev = genesis;  let mut ver_carry = 0u128;  // follower state (separate, same updates)
        let mut total = 0u128;
        let dt_us = 4_545u128;       // ~220 blk/s — a sub-millisecond block
        let n = 10_000u128;
        for i in 1..=n {
            let ts = genesis + i * dt_us;                           // block i's stamped µs ts
            let (produced, pc) = block_reward_time(genesis, prod_prev, ts, prod_carry);
            let (verified, vc) = block_reward_time(genesis, ver_prev, ts, ver_carry); // follower recompute
            assert_eq!(produced, verified, "produce/verify reward diverged at block {i} — would FORK");
            assert_eq!(pc, vc, "carry diverged at block {i} — would FORK");
            assert!(produced > 0, "a sub-millisecond block must NEVER truncate to a 0 reward");
            total += produced;
            prod_prev = ts; prod_carry = pc; ver_prev = ts; ver_carry = vc; // advance clock + carry
        }
        // EXACT-CARRY model: the sub-unit remainder is carried across blocks, so the chain total
        // equals the closed-form time integral EXACTLY — ZERO emission lost to truncation.
        let elapsed_us = n * dt_us;
        let integral = ((MAX_SUPPLY / 2) * elapsed_us) / EMISSION_DENOM;
        assert_eq!(total, integral, "with the carry, summed rewards == the time integral EXACTLY");
        assert!(total < MAX_SUPPLY);
    }

    // ── H7 timestamp-guard gate (mirrors the H1 test discipline) ─────────────────────
    const NOW: u128 = 1_782_517_293_832_369; // a real µs wall-clock instant

    #[test]
    fn h7_dormant_below_activation_is_a_noop() {
        // Realistic live height + an absurdly future stamp: with the default
        // u64::MAX activation the guard MUST pass — merging changes nothing live.
        let far_future = NOW + 365 * 86_400 * US_PER_SEC; // a year ahead
        assert!(check_block_ts(46_899, far_future, NOW).is_ok());
        assert!(check_block_ts(H7_TS_GUARD_ACTIVATION_HEIGHT - 1, far_future, NOW).is_ok());
    }

    #[test]
    fn h7_active_rejects_future_stamp_beyond_bound() {
        let h = H7_TS_GUARD_ACTIVATION_HEIGHT; // at/above activation = enforced
        let ts = NOW + MAX_FUTURE_DRIFT_US + 1;
        let err = check_block_ts(h, ts, NOW).unwrap_err();
        assert_eq!(err.excess_us, 1, "excess must measure past-the-bound exactly");
        assert_eq!(err.height, h);
    }

    #[test]
    fn h7_active_accepts_past_and_within_bound() {
        let h = H7_TS_GUARD_ACTIVATION_HEIGHT;
        assert!(check_block_ts(h, NOW - US_PER_SEC, NOW).is_ok(), "past stamps always pass");
        assert!(check_block_ts(h, NOW, NOW).is_ok(), "exactly-now passes");
        assert!(check_block_ts(h, NOW + MAX_FUTURE_DRIFT_US, NOW).is_ok(), "exact bound passes");
        assert!(check_block_ts(h, NOW + MAX_FUTURE_DRIFT_US + 1, NOW).is_err(), "bound + 1 µs rejects");
    }

    #[test]
    fn h7_predicate_is_ungated() {
        // The pure predicate ignores the activation gate — usable for telemetry now.
        assert!(ts_within_future_bound(NOW + MAX_FUTURE_DRIFT_US, NOW));
        assert!(!ts_within_future_bound(NOW + MAX_FUTURE_DRIFT_US + 1, NOW));
    }
}
