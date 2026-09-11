//! sigil-stake — **stake without revealing how much.**
//!
//! ```text
//!   Stake    → the note's nullifier enters the STAKE SET. The value is frozen, never disclosed.
//!   Prove    → "I hold a locked note in tier N" — a range proof, not an amount.
//!   Unstake  → request, wait out the cooldown, then release. Never instant.
//! ```
//!
//! ## The one property everything rests on
//!
//! **A staked note must be unspendable.** If a nullifier can sit in the stake set and still pass
//! the spend guard, a staker votes with value they then walk away with, and the security budget is
//! imaginary. So [`StakeSet::blocks_spend`] exists and the chain's spend path must consult it
//! alongside the spent set — the two together are the guard, and either alone is a hole.
//! [`tests::a_staked_note_cannot_be_spent`] is the test that says so.
//!
//! ## Why unstaking waits
//!
//! Instant unstake means vote-and-run: sign a bad checkpoint, release the stake, keep the value.
//! Slashing is only meaningful while there is something still locked to slash. The cooldown is not
//! friction for its own sake, it is the window in which misbehaviour can still cost something.
//!
//! ## What this crate deliberately does NOT do
//!
//! It does not generate or verify the range proof. That belongs in `sigil-shield`, next to the
//! spend circuit that already proves value relationships without disclosing values, and putting a
//! second proof system here would be inventing a parallel one. This crate holds the set, the
//! rules, the tiers and the cooldown — the parts that decide, not the parts that prove.
//!
//! It also does not slash. The penalty rule is a consensus decision and is not mine to pick; what
//! is here is the lock that makes a penalty *possible*, which is the part that has to exist first.

#![forbid(unsafe_code)]

pub mod tier;
pub use tier::{ladder, tier_of, Tier, TierError, MIN_STAKE, STEPS_PER_TIER};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A note's nullifier — the same 32 bytes the spend guard uses. Reused on purpose: the value
/// already has exactly one nullifier, so one note is one stake and one vote, with no new
/// uniqueness machinery to get wrong.
pub type Nullifier = [u8; 32];
/// Who the stake votes as. The validator's identity, not the note's owner — they may differ, and
/// keeping them separate is what lets a holder delegate without handing over spending power.
pub type ValidatorId = [u8; 32];

/// Blocks between requesting an unstake and being able to release it.
///
/// At the measured ~6 blk/s this is a little over a day, and it must comfortably exceed the
/// finality depth (512 blocks) or a staker could outrun the very settlement their vote decided.
pub const COOLDOWN_BLOCKS: u64 = 600_000;

const _: () = assert!(
    COOLDOWN_BLOCKS > 512 * 16,
    "cooldown must clear finality depth by a wide margin, or a vote can be outrun by its own unstake"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StakeStatus {
    /// Locked and voting.
    Active,
    /// Unstake requested; still locked, still slashable, no longer voting.
    Cooling { requested_at: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stake {
    pub nullifier: Nullifier,
    pub validator: ValidatorId,
    /// The weight class. **Never the amount** — nothing in this struct can be turned back into one.
    pub tier: Tier,
    pub staked_at: u64,
    pub status: StakeStatus,
}

impl Stake {
    /// Weight counts only while Active. A cooling stake keeps its lock (so it can still be
    /// punished) and loses its voice (so it cannot vote on its way out).
    pub fn weight(&self) -> u64 {
        match self.status {
            StakeStatus::Active => self.tier.weight(),
            StakeStatus::Cooling { .. } => 0,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StakeError {
    #[error("that note is already staked")]
    AlreadyStaked,
    #[error("that note is already spent — a spent note has no value left to lock")]
    AlreadySpent,
    #[error("no such stake")]
    NotStaked,
    #[error("unstake was not requested — request it, wait out the cooldown, then release")]
    NotCooling,
    #[error("cooldown has {remaining} block(s) left")]
    StillCooling { remaining: u64 },
    #[error(transparent)]
    Tier(#[from] TierError),
    #[error("this stake already voted at height {height}")]
    AlreadyVoted { height: u64 },
}

/// The stake set. Keyed by nullifier, so one note is one stake by construction.
/// Serialize the stake map as a LIST, not a map.
///
/// 🪤 Found by [`tests::the_set_never_stores_an_amount`], which tried to JSON-encode the set and
/// got `key must be a string`. A `BTreeMap<[u8; 32], _>` is not JSON-encodable at all, so the
/// derived `Serialize` on this struct compiled fine and could never actually run — the type would
/// have failed the first time the chain tried to persist or ship it. `Stake` already carries its
/// own `nullifier`, so a list loses nothing and round-trips everywhere.
mod stakes_as_list {
    use super::{Nullifier, Stake};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(m: &BTreeMap<Nullifier, Stake>, s: S) -> Result<S::Ok, S::Error> {
        m.values().cloned().collect::<Vec<Stake>>().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<Nullifier, Stake>, D::Error> {
        Ok(Vec::<Stake>::deserialize(d)?.into_iter().map(|s| (s.nullifier, s)).collect())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StakeSet {
    #[serde(with = "stakes_as_list")]
    stakes: BTreeMap<Nullifier, Stake>,
    /// (checkpoint height, nullifier) pairs already counted, so a stake votes once per height.
    voted: BTreeMap<u64, Vec<Nullifier>>,
}

impl StakeSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock a note. `amount` is used ONLY to derive the tier and is not retained anywhere.
    ///
    /// `already_spent` is the chain's spend guard, passed in rather than imported, so this crate
    /// never has to hold chain state to be correct — and so the caller cannot forget to consult it.
    pub fn stake(
        &mut self,
        nullifier: Nullifier,
        validator: ValidatorId,
        amount: u128,
        height: u64,
        already_spent: bool,
    ) -> Result<Tier, StakeError> {
        if already_spent {
            return Err(StakeError::AlreadySpent);
        }
        if self.stakes.contains_key(&nullifier) {
            return Err(StakeError::AlreadyStaked);
        }
        let tier = tier_of(amount)?;
        self.stakes.insert(
            nullifier,
            Stake { nullifier, validator, tier, staked_at: height, status: StakeStatus::Active },
        );
        Ok(tier)
    }

    /// **The guard.** True while the note is locked, in either status — a cooling stake is still
    /// locked, which is the entire reason a cooldown deters anything.
    pub fn blocks_spend(&self, nullifier: &Nullifier) -> bool {
        self.stakes.contains_key(nullifier)
    }

    /// Begin unstaking. The stake stops voting immediately and stays locked.
    pub fn request_unstake(&mut self, nullifier: &Nullifier, height: u64) -> Result<u64, StakeError> {
        let s = self.stakes.get_mut(nullifier).ok_or(StakeError::NotStaked)?;
        s.status = StakeStatus::Cooling { requested_at: height };
        Ok(height + COOLDOWN_BLOCKS)
    }

    /// Release a cooled stake, freeing the note to be spent again.
    pub fn release(&mut self, nullifier: &Nullifier, height: u64) -> Result<Tier, StakeError> {
        let s = self.stakes.get(nullifier).ok_or(StakeError::NotStaked)?;
        let requested_at = match s.status {
            StakeStatus::Cooling { requested_at } => requested_at,
            StakeStatus::Active => return Err(StakeError::NotCooling),
        };
        let ready_at = requested_at + COOLDOWN_BLOCKS;
        if height < ready_at {
            return Err(StakeError::StillCooling { remaining: ready_at - height });
        }
        let tier = s.tier;
        self.stakes.remove(nullifier);
        Ok(tier)
    }

    /// Count a stake's vote at a checkpoint height, once.
    pub fn vote(&mut self, nullifier: &Nullifier, height: u64) -> Result<u64, StakeError> {
        let s = self.stakes.get(nullifier).ok_or(StakeError::NotStaked)?;
        let w = s.weight();
        let at = self.voted.entry(height).or_default();
        if at.contains(nullifier) {
            return Err(StakeError::AlreadyVoted { height });
        }
        at.push(*nullifier);
        Ok(w)
    }

    /// Total active weight — the number consensus divides to find a quorum.
    pub fn total_weight(&self) -> u64 {
        self.stakes.values().map(|s| s.weight()).sum()
    }

    pub fn get(&self, nullifier: &Nullifier) -> Option<&Stake> {
        self.stakes.get(nullifier)
    }
    pub fn len(&self) -> usize {
        self.stakes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.stakes.is_empty()
    }

    /// How many stakes share each tier — the anonymity set, measured rather than asserted. A tier
    /// with one member hides nobody, and an honest UI should say so before someone stakes into it.
    pub fn crowd(&self) -> BTreeMap<Tier, usize> {
        let mut m = BTreeMap::new();
        for s in self.stakes.values() {
            *m.entry(s.tier).or_insert(0) += 1;
        }
        m
    }

    /// Commitment over the whole set, hashed field by field.
    ///
    /// 🪤 Not `serde_json` — this map is keyed by `[u8; 32]`, serde_json refuses non-string map
    /// keys, and an `unwrap_or_default()` there would make this root a silent CONSTANT. That exact
    /// fault shipped in `sigil-court` and was caught only by a test that asserted the root MOVES.
    pub fn stake_root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-stake/root/v1");
        h.update(&(self.stakes.len() as u64).to_le_bytes());
        for (nf, s) in &self.stakes {
            h.update(nf);
            h.update(&s.validator);
            h.update(&[s.tier.0]);
            h.update(&s.staked_at.to_le_bytes());
            match s.status {
                StakeStatus::Active => h.update(&[0u8]),
                StakeStatus::Cooling { requested_at } => {
                    h.update(&[1u8]);
                    h.update(&requested_at.to_le_bytes())
                }
            };
        }
        *h.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nf(b: u8) -> Nullifier { [b; 32] }
    fn v(b: u8) -> ValidatorId { [b; 32] }
    fn amt() -> u128 { *ladder().first().unwrap() }

    /// THE property. If this ever fails, the security budget is imaginary: a staker would vote
    /// with value and then spend it.
    #[test]
    fn a_staked_note_cannot_be_spent() {
        let mut s = StakeSet::new();
        assert!(!s.blocks_spend(&nf(1)));
        s.stake(nf(1), v(9), amt(), 100, false).unwrap();
        assert!(s.blocks_spend(&nf(1)), "an active stake must block the spend");

        // And it STAYS blocked while cooling — that is what makes a penalty possible at all.
        s.request_unstake(&nf(1), 200).unwrap();
        assert!(s.blocks_spend(&nf(1)), "a cooling stake is still locked");

        // Only a completed release frees it.
        s.release(&nf(1), 200 + COOLDOWN_BLOCKS).unwrap();
        assert!(!s.blocks_spend(&nf(1)));
    }

    #[test]
    fn an_already_spent_note_cannot_be_staked() {
        let mut s = StakeSet::new();
        assert_eq!(s.stake(nf(2), v(9), amt(), 1, true), Err(StakeError::AlreadySpent));
        assert!(s.is_empty());
    }

    #[test]
    fn unstaking_cannot_be_rushed_and_silences_the_vote_at_once() {
        let mut s = StakeSet::new();
        s.stake(nf(3), v(9), amt(), 10, false).unwrap();
        assert!(s.total_weight() > 0);

        // Release before requesting is refused outright.
        assert_eq!(s.release(&nf(3), 10), Err(StakeError::NotCooling));

        s.request_unstake(&nf(3), 50).unwrap();
        // The voice goes immediately...
        assert_eq!(s.total_weight(), 0, "a cooling stake must not vote on its way out");
        // ...the lock does not.
        assert_eq!(
            s.release(&nf(3), 50 + COOLDOWN_BLOCKS - 1),
            Err(StakeError::StillCooling { remaining: 1 })
        );
        assert!(s.release(&nf(3), 50 + COOLDOWN_BLOCKS).is_ok());
    }

    #[test]
    fn one_note_is_one_stake_and_one_vote_per_height() {
        let mut s = StakeSet::new();
        s.stake(nf(4), v(9), amt(), 1, false).unwrap();
        assert_eq!(s.stake(nf(4), v(9), amt(), 1, false), Err(StakeError::AlreadyStaked));

        assert!(s.vote(&nf(4), 900).is_ok());
        assert_eq!(s.vote(&nf(4), 900), Err(StakeError::AlreadyVoted { height: 900 }));
        // A different checkpoint is a different vote, which is correct.
        assert!(s.vote(&nf(4), 901).is_ok());
    }

    #[test]
    fn the_set_never_stores_an_amount() {
        let mut s = StakeSet::new();
        let big = *ladder().last().unwrap();
        s.stake(nf(5), v(9), big, 1, false).unwrap();
        let json = serde_json::to_string(&s).expect("the set must actually be encodable");
        assert!(
            !json.contains(&big.to_string()),
            "the staked amount must not survive anywhere in the set — it is the thing being hidden"
        );
        // And it must come back the same, or persistence silently loses stakes.
        let back: StakeSet = serde_json::from_str(&json).expect("and decodable");
        assert_eq!(back.stake_root(), s.stake_root(), "a round trip must preserve the set exactly");
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn the_crowd_is_measurable_so_a_thin_tier_can_be_admitted_to() {
        let mut s = StakeSet::new();
        let a = amt();
        for i in 0..5u8 { s.stake(nf(10 + i), v(i), a, 1, false).unwrap(); }
        let t = tier_of(a).unwrap();
        assert_eq!(s.crowd().get(&t), Some(&5), "five stakers hide each other; one hides nobody");
    }

    #[test]
    fn the_root_moves_for_every_change() {
        let mut s = StakeSet::new();
        let empty = s.stake_root();
        s.stake(nf(6), v(9), amt(), 1, false).unwrap();
        let staked = s.stake_root();
        assert_ne!(staked, empty, "staking must move the root");
        s.request_unstake(&nf(6), 5).unwrap();
        assert_ne!(s.stake_root(), staked, "cooling must move the root");
        s.release(&nf(6), 5 + COOLDOWN_BLOCKS).unwrap();
        assert_eq!(s.stake_root(), empty, "and releasing returns it");
    }
}
