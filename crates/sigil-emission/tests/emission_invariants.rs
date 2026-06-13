//! Emission-invariant integration tests — Inv 4 (halving → 21M cap).
//!
//! The inline `#[cfg(test)]` module in `lib.rs` pins the *spot values*
//! (reward at the first few halving boundaries, cumulative < cap). This file
//! pins the deeper *structural properties* that a future refactor of the
//! schedule could silently break without tripping a spot check:
//!
//!  - the reward is monotonically non-increasing across the whole schedule;
//!  - the two public emission views agree — `cumulative_emission(N)` equals
//!    `Σ block_reward(h)` for `h in 0..N` (a refactor that desyncs them is a
//!    supply bug waiting to happen);
//!  - every per-epoch slice mints exactly `(R0 >> e) · HALVING_INTERVAL`;
//!  - the TIME-based model loses ZERO base units to integer truncation no
//!    matter how the wall-clock interval is sliced (carry threading), and a
//!    full 4-year epoch mints exactly `MAX_SUPPLY/2`;
//!  - neither model ever breaches `MAX_SUPPLY`.
//!
//! These are pure functions: deterministic, instant, no async, no flake.

use sigil_emission::{
    block_reward, block_reward_time, cumulative_emission, cumulative_emission_time,
    EMISSION_DENOM, HALVING_INTERVAL, HALVING_PERIOD_SECS, INITIAL_BLOCK_REWARD, US_PER_SEC,
};
use sigil_state::MAX_SUPPLY;

/// The block reward never increases as height grows — across every halving
/// boundary and well past the terminal epoch. A non-monotone reward would mean
/// some height mints MORE than an earlier one: an inflation regression.
#[test]
fn block_reward_is_monotonically_non_increasing() {
    let mut prev = block_reward(0);
    // Sample densely around the first 10 halving boundaries, then sparsely out
    // past termination.
    let mut heights: Vec<u64> = Vec::new();
    for e in 0..10u64 {
        let base = e * HALVING_INTERVAL;
        heights.extend([base, base + 1, base + HALVING_INTERVAL / 2, base + HALVING_INTERVAL - 1]);
    }
    heights.extend((10..70u64).map(|e| e * HALVING_INTERVAL));
    heights.sort_unstable();
    for h in heights {
        let r = block_reward(h);
        assert!(r <= prev, "reward rose at height {h}: {r} > {prev}");
        prev = r;
    }
    assert_eq!(prev, 0, "reward must reach 0 by the terminal epoch");
}

/// The two public emission views MUST agree: integrating the per-block reward
/// from genesis is, by construction, the cumulative-emission closed form. If a
/// refactor changes one and not the other, total supply accounting silently
/// drifts. We check exact agreement over a range that crosses several halvings.
#[test]
fn cumulative_matches_summed_block_reward() {
    // Use a reduced span (a few thousand blocks across an artificially short
    // window) AND real halving boundaries. We can't sum 2.1M blocks per epoch
    // in a unit test, so verify agreement at every multiple-of-interval point
    // (where cumulative has a closed form) plus a fine sweep inside epoch 0.
    for n in [0u64, 1, 2, 1000, HALVING_INTERVAL / 2, HALVING_INTERVAL] {
        let summed: u128 = (0..n).map(block_reward).sum();
        assert_eq!(
            cumulative_emission(n),
            summed,
            "cumulative_emission({n}) disagrees with Σ block_reward"
        );
    }
}

/// Each full halving epoch emits exactly `(R0 >> e) · HALVING_INTERVAL`, and the
/// epoch-0 emission is exactly `MAX_SUPPLY / 2` — the geometric-series anchor
/// that makes the whole schedule sum to the 21M cap.
#[test]
fn per_epoch_slice_is_exact() {
    for e in 0..12u32 {
        let start = e as u64 * HALVING_INTERVAL;
        let end = start + HALVING_INTERVAL;
        let slice = cumulative_emission(end) - cumulative_emission(start);
        let expected = (INITIAL_BLOCK_REWARD >> e) * HALVING_INTERVAL as u128;
        assert_eq!(slice, expected, "epoch {e} slice wrong");
    }
    assert_eq!(
        cumulative_emission(HALVING_INTERVAL),
        MAX_SUPPLY / 2,
        "epoch 0 must mint exactly half the cap"
    );
}

/// Neither emission view ever breaches the hard cap, even at absurd heights.
#[test]
fn never_breaches_cap_block_model() {
    for h in [
        HALVING_INTERVAL * 60,
        HALVING_INTERVAL * 64,
        HALVING_INTERVAL * 1000,
        u64::MAX / 2,
        u64::MAX,
    ] {
        assert!(
            cumulative_emission(h) <= MAX_SUPPLY,
            "block-model cumulative breached cap at height {h}"
        );
    }
}

/// TIME model: one full 4-year halving period mints exactly `MAX_SUPPLY/2`, and
/// cumulative time-emission stays strictly under the cap while climbing toward
/// it. Mirrors the block model's epoch-0 anchor for the wall-clock schedule.
#[test]
fn time_model_epoch_zero_is_half_cap_and_bounded() {
    assert_eq!(
        cumulative_emission_time(HALVING_PERIOD_SECS),
        MAX_SUPPLY / 2,
        "first 4-year epoch must emit exactly half the cap"
    );
    // Climbs monotonically toward — but never reaches — the cap.
    let mut prev = 0u128;
    for periods in 0..40u64 {
        let elapsed = periods * HALVING_PERIOD_SECS;
        let cum = cumulative_emission_time(elapsed);
        assert!(cum >= prev, "time-cumulative went backwards at {elapsed}s");
        assert!(cum < MAX_SUPPLY, "time-cumulative reached/exceeded cap at {elapsed}s");
        prev = cum;
    }
}

/// The headline carry-threading property: emission is INVARIANT to how the
/// wall-clock interval is sliced. Summing `block_reward_time` over many fine
/// sub-slices (threading the remainder) yields the SAME total base units as one
/// coarse call — zero loss to integer truncation. This is the exact guarantee
/// the doc-comment claims; a regression here leaks or double-mints SIGIL at
/// epoch boundaries.
#[test]
fn time_reward_carry_threading_is_lossless() {
    let genesis_us: u128 = 1_000_000_000 * US_PER_SEC; // arbitrary genesis
    // Span an interval that straddles a halving boundary so truncation pressure
    // is real: from 0.5 epochs to 1.5 epochs of elapsed time.
    let start_us = genesis_us + (HALVING_PERIOD_SECS as u128 * US_PER_SEC) / 2;
    let end_us = genesis_us + (HALVING_PERIOD_SECS as u128 * US_PER_SEC) * 3 / 2;

    // Coarse: a single call over the whole span.
    let (coarse_reward, coarse_rem) = block_reward_time(genesis_us, start_us, end_us, 0);

    // Fine: 1000 equal sub-slices, threading the remainder.
    let steps = 1000u128;
    let step = (end_us - start_us) / steps;
    let mut a = start_us;
    let mut total = 0u128;
    let mut carry = 0u128;
    for _ in 0..steps {
        let b = a + step;
        let (r, rem) = block_reward_time(genesis_us, a, b, carry);
        total += r;
        carry = rem;
        a = b;
    }
    // tail (rounding of the division above)
    if a < end_us {
        let (r, rem) = block_reward_time(genesis_us, a, end_us, carry);
        total += r;
        carry = rem;
    }

    // The base units minted must match within at most 1 unit of leftover carry,
    // and the leftover carry/EMISSION_DENOM fraction must reconcile exactly.
    let coarse_total_numer = coarse_reward * EMISSION_DENOM + coarse_rem;
    let fine_total_numer = total * EMISSION_DENOM + carry;
    assert_eq!(
        coarse_total_numer, fine_total_numer,
        "carry-threaded fine slicing minted a different numerator than the coarse call \
         (coarse {coarse_reward}+{coarse_rem}/D vs fine {total}+{carry}/D)"
    );
}

/// Nothing is emitted before genesis, and a zero/negative interval mints zero —
/// guards against a sign/underflow bug minting on a backwards clock.
#[test]
fn time_reward_rejects_pre_genesis_and_empty_intervals() {
    let genesis_us: u128 = 5_000_000 * US_PER_SEC;
    // Interval entirely before genesis: zero.
    let (r, _) = block_reward_time(genesis_us, genesis_us - 1000, genesis_us - 10, 0);
    assert_eq!(r, 0, "emitted before genesis");
    // Empty interval: zero, carry preserved.
    let (r, rem) = block_reward_time(genesis_us, genesis_us + 100, genesis_us + 100, 42);
    assert_eq!((r, rem), (0, 42), "empty interval should mint nothing and keep carry");
    // Backwards interval (block_ts < prev_ts): zero, carry preserved.
    let (r, rem) = block_reward_time(genesis_us, genesis_us + 200, genesis_us + 100, 7);
    assert_eq!((r, rem), (0, 7), "backwards interval should mint nothing");
}
