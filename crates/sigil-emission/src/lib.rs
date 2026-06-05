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
}
