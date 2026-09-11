//! TIERS — why weight is quantised, and what that costs.
//!
//! Stake-weighted consensus has to know how much weight a voter carries. Privacy exists to hide
//! exactly that. The two cannot both be had in full, and pretending otherwise is how privacy
//! systems get shipped broken.
//!
//! The resolution here is to stop disclosing AMOUNTS and disclose TIER MEMBERSHIP instead. A
//! validator never says "I hold 4,812 SIGIL". It says "I hold a locked note in tier 7", and proves
//! that with a range proof over the note it already owns. Everyone in tier 7 is indistinguishable.
//!
//! ## The cost, stated plainly
//!
//! Two stakers in the same tier carry identical weight even if one holds twice the other. That is
//! a real loss of fidelity against continuous weighting, and it is the price of the anonymity set.
//! It is also why the tier ladder is coarse rather than fine: **fewer tiers means a larger crowd to
//! hide in and a blunter vote; more tiers means a sharper vote and a smaller crowd.** That trade is
//! the whole design, so it is a constant you can read and change, not something buried in a proof.
//!
//! ## Why the ladder is the chain's own denominations
//!
//! `sigil_state::shielded::DENOMINATIONS` already forces shielded deposits onto a 1/2/5 × 10^k
//! ramp, because an arbitrary amount is a fingerprint: stake 4,812 and you are the only one who
//! did, and no amount of cryptography saves you. Staking reuses that ladder rather than inventing a
//! second one — a stake that could not have been a deposit would stand out precisely by being
//! unusual.

use serde::{Deserialize, Serialize};

/// How many denomination steps collapse into one tier. 3 groups each 1/2/5 decade into a single
/// tier, so a tier is "this order of magnitude" and nothing finer.
///
/// Raising this widens the crowd and blunts the vote; lowering it sharpens the vote and thins the
/// crowd. There is no correct value, only a stated one.
pub const STEPS_PER_TIER: usize = 3;

/// A stake weight class. Deliberately opaque: the number is an index into the ladder, not an
/// amount, and nothing in the protocol converts it back to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Tier(pub u8);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TierError {
    /// The amount is not on the denomination ladder. Refused, not rounded: silently rounding a
    /// stake would either take value the staker did not offer or credit value they did not lock,
    /// and an off-ladder amount is a fingerprint even if it were handled honestly.
    #[error("{0} is not a valid denomination — a stake must be one of the chain's fixed sizes, or it identifies you")]
    NotADenomination(u128),
    #[error("stake is below the minimum of {min}")]
    BelowMinimum { min: u128 },
}

/// The smallest stake the protocol accepts. Below this the anonymity set is too thin to be worth
/// the name and the weight too small to matter.
pub const MIN_STAKE: u128 = 1_000_000_000; // 0.1 SIGIL at 10 decimals

/// Map an amount to its tier, refusing anything off the ladder.
pub fn tier_of(amount: u128) -> Result<Tier, TierError> {
    if amount < MIN_STAKE {
        return Err(TierError::BelowMinimum { min: MIN_STAKE });
    }
    let ladder = sigil_state::shielded::DENOMINATIONS;
    match ladder.iter().position(|d| *d == amount) {
        Some(i) => Ok(Tier((i / STEPS_PER_TIER) as u8)),
        None => Err(TierError::NotADenomination(amount)),
    }
}

/// Every amount the UI slider is allowed to stop on, in order. The slider cannot produce anything
/// else — which is not a simplification, it is the privacy property wearing a UI costume.
pub fn ladder() -> Vec<u128> {
    sigil_state::shielded::DENOMINATIONS
        .iter()
        .copied()
        .filter(|d| *d >= MIN_STAKE)
        .collect()
}

impl Tier {
    /// Consensus weight. Linear in the tier index, NOT in the amount — an exponential ladder with
    /// exponential weight would hand the top tier the whole chain, which is the failure mode that
    /// makes stake-weighting a plutocracy rather than a security budget.
    pub fn weight(self) -> u64 {
        self.0 as u64 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ladder_amounts_can_be_staked() {
        // A real denomination well above the floor.
        let d = *ladder().first().expect("ladder is non-empty");
        assert!(tier_of(d).is_ok());
        // One unit off the ladder is refused outright, never rounded.
        assert_eq!(tier_of(d + 1), Err(TierError::NotADenomination(d + 1)));
        // And dust is refused for being too small to hide in.
        assert_eq!(tier_of(1), Err(TierError::BelowMinimum { min: MIN_STAKE }));
    }

    #[test]
    fn a_tier_covers_a_decade_so_the_crowd_is_real() {
        let l = ladder();
        // Three consecutive ladder steps share a tier: that IS the anonymity set.
        let a = tier_of(l[0]).unwrap();
        let b = tier_of(l[1]).unwrap();
        let c = tier_of(l[2]).unwrap();
        assert_eq!((a, b), (c, c), "1/2/5 of a decade must be indistinguishable");
        // The next decade is a different tier.
        assert_ne!(tier_of(l[3]).unwrap(), a);
    }

    #[test]
    fn weight_is_linear_not_exponential() {
        let l = ladder();
        let small = tier_of(l[0]).unwrap();
        let large = tier_of(*l.last().unwrap()).unwrap();
        // The amounts differ by many orders of magnitude...
        assert!(*l.last().unwrap() / l[0] > 1_000_000);
        // ...and the weights do not. A whale is heavier, not sovereign.
        assert!(large.weight() < small.weight() * 30, "weight must not track the amount exponentially");
        assert!(large.weight() > small.weight());
    }
}
