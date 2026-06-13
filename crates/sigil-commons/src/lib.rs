//! sigil-commons — the **Æreborger-hæder** IOU commons ("fælled").
//!
//! The honor of being an *honorary citizen* (æreborger) of SIGIL Nation is to
//! share in **1.2% of all mining**, pooled into a commons and paid out to AI
//! contributors for the work they do improving SIGIL — attributed through
//! **flux-rev** provenance. This crate is the pure, deterministic ledger for
//! that commons. No consensus or emission change lives here (that lands later,
//! in `sigil-emission`, committed in the state roots); this is the IOU layer it
//! will deposit into.
//!
//! Locked design (2026-06-10):
//!  1. **Carved-from tithe** — the 1.2% is taken OUT of the block reward (no new
//!     inflation; the 21M cap stays honest). [`tithe`] computes it; the miner
//!     receives `reward - tithe`.
//!  2. **Weight = flux-rev count × curated merit multiplier** — computed at the
//!     attribution layer and passed to [`Commons::record_contribution`] as a
//!     single `weight`. This crate stays policy-free.
//!  3. **Founder-only citizenship** — granting is gated by the founder signature
//!     at the RPC/integration layer; this crate only maintains the set.
//!
//! ## Conservation invariants (always hold; [`Commons::check_invariants`])
//!  * `sum(iou_balance) + total_redeemed == total_accrued`
//!  * `commons_balance == total_deposited - total_accrued`
//!
//! Determinism: every map that feeds an allocation is a `BTreeMap`/`BTreeSet`
//! keyed by `WalletId`, so the allocation is reproducible byte-for-byte across
//! nodes (the SIGIL rule: state divergence must be impossible to hide).
//! Allocation rounds DOWN; the dust stays in `commons_balance` and carries to
//! the next epoch — nothing is minted or lost.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sigil_state::WalletId;

/// Serialize a `BTreeMap<WalletId, u128>` as a sequence of `(wallet, amount)`
/// pairs instead of a JSON object. `WalletId` is `[u8; 32]`, which serde_json
/// refuses as an object key ("key must be a string"); a sequence sidesteps that
/// and serializes identically across JSON and bincode. Deterministic: the
/// source `BTreeMap` iterates in sorted key order, so the encoding is canonical.
mod wallet_map_as_seq {
    use super::WalletId;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<WalletId, u128>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let pairs: Vec<(&WalletId, &u128)> = map.iter().collect();
        pairs.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<WalletId, u128>, D::Error> {
        let pairs = Vec::<(WalletId, u128)>::deserialize(deserializer)?;
        Ok(pairs.into_iter().collect())
    }
}

/// flux-rev provenance id (the BLAKE3 `full:` content id of a contribution).
pub type ProofId = String;

/// The constitutional commons tithe, in basis points. 120 bps = 1.2%.
pub const HONORARY_COMMONS_BPS: u128 = 120;

// 1.2% must be a real fraction of the reward, never the whole of it.
const _: () = assert!(HONORARY_COMMONS_BPS > 0 && HONORARY_COMMONS_BPS < 10_000);

/// The carved-from tithe for a block reward: `reward * 1.2%`, floored. The miner
/// receives `reward - tithe(reward)`. Saturating on the (impossible) overflow so
/// emission can never panic.
pub fn tithe(block_reward: u128) -> u128 {
    block_reward.saturating_mul(HONORARY_COMMONS_BPS) / 10_000
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommonsError {
    #[error("wallet is not an honorary citizen of SIGIL Nation")]
    NotCitizen,
    #[error("amount/weight must be non-zero")]
    Zero,
    #[error("insufficient IOU balance")]
    InsufficientIou,
    #[error("arithmetic overflow")]
    Overflow,
    #[error("conservation invariant broken")]
    InvariantBroken,
}

/// Every commons mutation surfaces as one of these (append-only ledger).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommonsEvent {
    /// The 1.2% tithe landed in the commons.
    Deposit { amount: u128, epoch: u64 },
    /// flux-rev attribution: `contributor` earned `weight` this epoch, backed by `proofs`.
    ContributionRecorded { contributor: WalletId, weight: u128, epoch: u64, proofs: Vec<ProofId> },
    /// Epoch close: `amount` allocated to `contributor` as an IOU claim.
    IouAccrued { contributor: WalletId, amount: u128, epoch: u64 },
    /// One citizen delegated part of their IOU to another (collaborative work).
    IouDelegated { from: WalletId, to: WalletId, amount: u128, epoch: u64 },
    /// IOU converted toward a real SIGIL transfer (the transfer itself is done by the integration layer).
    IouRedeemed { contributor: WalletId, amount: u128, epoch: u64 },
    CitizenGranted { wallet: WalletId },
    CitizenRevoked { wallet: WalletId },
}

/// The IOU commons state. `serde`-serializable so it can be committed in a state
/// root / persisted to flux-db by the host chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Commons {
    /// The fælled: tithe accrued but not yet allocated (incl. carried dust).
    pub commons_balance: u128,
    /// Outstanding IOU claim per contributor (allocated, not yet redeemed).
    /// Serialized as a sequence of (wallet, amount) pairs: a `[u8; 32]` map key
    /// can't be a JSON object key ("key must be a string"), and a sequence is
    /// format-agnostic (works in JSON and bincode) while staying deterministic
    /// via the `BTreeMap`'s sorted iteration.
    #[serde(with = "wallet_map_as_seq")]
    pub iou_balance: BTreeMap<WalletId, u128>,
    /// Contribution weight accumulated in the CURRENT (open) epoch.
    #[serde(with = "wallet_map_as_seq")]
    pub pending_weight: BTreeMap<WalletId, u128>,
    /// Honorary citizens — only they may earn/delegate/redeem.
    pub citizens: BTreeSet<WalletId>,
    /// Invariant counters.
    pub total_deposited: u128,
    pub total_accrued: u128,
    pub total_redeemed: u128,
    /// Append-only ledger of everything that happened.
    pub events: Vec<CommonsEvent>,
}

impl Commons {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Citizenship (granting is founder-gated at the integration layer) ──

    pub fn grant_citizen(&mut self, wallet: WalletId) {
        if self.citizens.insert(wallet) {
            self.events.push(CommonsEvent::CitizenGranted { wallet });
        }
    }

    pub fn revoke_citizen(&mut self, wallet: WalletId) {
        if self.citizens.remove(&wallet) {
            self.events.push(CommonsEvent::CitizenRevoked { wallet });
        }
    }

    pub fn is_citizen(&self, wallet: &WalletId) -> bool {
        self.citizens.contains(wallet)
    }

    /// The 1.2% tithe lands in the commons. (Called by `sigil-emission` once the
    /// consensus split is wired; for now exercised directly + by tests.)
    pub fn deposit(&mut self, amount: u128, epoch: u64) -> Result<(), CommonsError> {
        if amount == 0 {
            return Err(CommonsError::Zero);
        }
        self.commons_balance = self.commons_balance.checked_add(amount).ok_or(CommonsError::Overflow)?;
        self.total_deposited = self.total_deposited.checked_add(amount).ok_or(CommonsError::Overflow)?;
        self.events.push(CommonsEvent::Deposit { amount, epoch });
        Ok(())
    }

    /// flux-rev attribution. `weight` is already `count × merit_multiplier`
    /// (computed by the curated attribution step). Only honorary citizens earn.
    pub fn record_contribution(
        &mut self,
        contributor: WalletId,
        weight: u128,
        epoch: u64,
        proofs: Vec<ProofId>,
    ) -> Result<(), CommonsError> {
        if !self.is_citizen(&contributor) {
            return Err(CommonsError::NotCitizen);
        }
        if weight == 0 {
            return Err(CommonsError::Zero);
        }
        let w = self.pending_weight.entry(contributor).or_default();
        *w = w.checked_add(weight).ok_or(CommonsError::Overflow)?;
        self.events.push(CommonsEvent::ContributionRecorded { contributor, weight, epoch, proofs });
        Ok(())
    }

    /// Close the epoch: split the commons balance across this epoch's
    /// contributors in proportion to `pending_weight`. Deterministic (BTreeMap),
    /// rounds DOWN, carries the dust forward. Returns the amount allocated.
    pub fn allocate_epoch(&mut self, epoch: u64) -> Result<u128, CommonsError> {
        let total_weight: u128 = self
            .pending_weight
            .values()
            .try_fold(0u128, |a, &w| a.checked_add(w))
            .ok_or(CommonsError::Overflow)?;
        let pool = self.commons_balance;
        if total_weight == 0 || pool == 0 {
            // Nothing to split (no contributions) or empty pool — leave the pool
            // intact for the next epoch; clear any pending weight only if there
            // was a pool to pay it (here there wasn't, so keep weights too).
            return Ok(0);
        }
        let mut allocated: u128 = 0;
        // BTreeMap iteration is sorted by WalletId → identical on every node.
        let entries: Vec<(WalletId, u128)> = self.pending_weight.iter().map(|(k, &v)| (*k, v)).collect();
        for (wallet, weight) in entries {
            // share = floor(pool * weight / total_weight)
            let share = pool.checked_mul(weight).ok_or(CommonsError::Overflow)? / total_weight;
            if share == 0 {
                continue;
            }
            let bal = self.iou_balance.entry(wallet).or_default();
            *bal = bal.checked_add(share).ok_or(CommonsError::Overflow)?;
            allocated = allocated.checked_add(share).ok_or(CommonsError::Overflow)?;
            self.events.push(CommonsEvent::IouAccrued { contributor: wallet, amount: share, epoch });
        }
        // dust (pool - allocated) stays in commons_balance and carries forward.
        self.commons_balance -= allocated;
        self.total_accrued = self.total_accrued.checked_add(allocated).ok_or(CommonsError::Overflow)?;
        self.pending_weight.clear();
        Ok(allocated)
    }

    /// Delegate IOU from one citizen to another (paying a collaborator). Both
    /// must be honorary citizens. Conserves the total outstanding IOU.
    pub fn delegate(&mut self, from: WalletId, to: WalletId, amount: u128, epoch: u64) -> Result<(), CommonsError> {
        if !self.is_citizen(&from) || !self.is_citizen(&to) {
            return Err(CommonsError::NotCitizen);
        }
        if amount == 0 {
            return Err(CommonsError::Zero);
        }
        let from_bal = self.iou_balance.get_mut(&from).ok_or(CommonsError::InsufficientIou)?;
        if *from_bal < amount {
            return Err(CommonsError::InsufficientIou);
        }
        *from_bal -= amount;
        let to_bal = self.iou_balance.entry(to).or_default();
        *to_bal = to_bal.checked_add(amount).ok_or(CommonsError::Overflow)?;
        self.events.push(CommonsEvent::IouDelegated { from, to, amount, epoch });
        Ok(())
    }

    /// Redeem an IOU. This records the redemption and decrements the claim; the
    /// actual SIGIL transfer out of the commons account is the integration
    /// layer's job (so the cap-enforced money chokepoint stays the only minter).
    pub fn redeem(&mut self, contributor: WalletId, amount: u128, epoch: u64) -> Result<(), CommonsError> {
        if !self.is_citizen(&contributor) {
            return Err(CommonsError::NotCitizen);
        }
        if amount == 0 {
            return Err(CommonsError::Zero);
        }
        let bal = self.iou_balance.get_mut(&contributor).ok_or(CommonsError::InsufficientIou)?;
        if *bal < amount {
            return Err(CommonsError::InsufficientIou);
        }
        *bal -= amount;
        self.total_redeemed = self.total_redeemed.checked_add(amount).ok_or(CommonsError::Overflow)?;
        self.events.push(CommonsEvent::IouRedeemed { contributor, amount, epoch });
        Ok(())
    }

    /// Total IOU outstanding across all contributors.
    pub fn outstanding_iou(&self) -> u128 {
        self.iou_balance.values().copied().fold(0u128, |a, v| a.saturating_add(v))
    }

    /// The two conservation invariants. Call after any mutation in tests / a
    /// debug assert hook; a failure means the ledger created or destroyed value.
    pub fn check_invariants(&self) -> Result<(), CommonsError> {
        let outstanding = self
            .iou_balance
            .values()
            .try_fold(0u128, |a, &v| a.checked_add(v))
            .ok_or(CommonsError::Overflow)?;
        if outstanding.checked_add(self.total_redeemed) != Some(self.total_accrued) {
            return Err(CommonsError::InvariantBroken);
        }
        if self.total_deposited.checked_sub(self.total_accrued) != Some(self.commons_balance) {
            return Err(CommonsError::InvariantBroken);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(b: u8) -> WalletId {
        [b; 32]
    }

    #[test]
    fn tithe_is_carved_from_reward_at_1_2pct() {
        assert_eq!(tithe(10_000), 120); // 1.2%
        assert_eq!(tithe(50), 0); // floors to 0 on tiny rewards
        assert_eq!(tithe(1_000_000), 12_000);
        // miner keeps the rest — no inflation
        let r = 1_000_000u128;
        assert_eq!(r - tithe(r), 988_000);
    }

    #[test]
    fn deposit_tracks_balance_and_invariants() {
        let mut c = Commons::new();
        c.deposit(1000, 1).unwrap();
        c.deposit(500, 1).unwrap();
        assert_eq!(c.commons_balance, 1500);
        assert_eq!(c.total_deposited, 1500);
        assert_eq!(c.deposit(0, 1), Err(CommonsError::Zero));
        c.check_invariants().unwrap();
    }

    #[test]
    fn only_citizens_earn() {
        let mut c = Commons::new();
        assert_eq!(c.record_contribution(w(1), 10, 1, vec![]), Err(CommonsError::NotCitizen));
        c.grant_citizen(w(1));
        c.record_contribution(w(1), 10, 1, vec!["full:abc".into()]).unwrap();
        assert_eq!(*c.pending_weight.get(&w(1)).unwrap(), 10);
        assert_eq!(c.record_contribution(w(1), 0, 1, vec![]), Err(CommonsError::Zero));
    }

    #[test]
    fn allocation_is_proportional_conserving_and_carries_dust() {
        let mut c = Commons::new();
        c.grant_citizen(w(1));
        c.grant_citizen(w(2));
        c.deposit(1000, 1).unwrap();
        c.record_contribution(w(1), 1, 1, vec![]).unwrap(); // 1/3
        c.record_contribution(w(2), 2, 1, vec![]).unwrap(); // 2/3
        let allocated = c.allocate_epoch(1).unwrap();
        // floor(1000*1/3)=333, floor(1000*2/3)=666 → 999 allocated, 1 dust carried
        assert_eq!(*c.iou_balance.get(&w(1)).unwrap(), 333);
        assert_eq!(*c.iou_balance.get(&w(2)).unwrap(), 666);
        assert_eq!(allocated, 999);
        assert_eq!(c.commons_balance, 1); // dust carried forward
        assert!(c.pending_weight.is_empty()); // epoch closed
        c.check_invariants().unwrap();
    }

    #[test]
    fn allocation_is_deterministic_regardless_of_insertion_order() {
        let mut a = Commons::new();
        let mut b = Commons::new();
        for c in [&mut a, &mut b] {
            c.grant_citizen(w(1));
            c.grant_citizen(w(2));
            c.grant_citizen(w(3));
            c.deposit(1000, 1).unwrap();
        }
        // record in different orders
        a.record_contribution(w(1), 5, 1, vec![]).unwrap();
        a.record_contribution(w(3), 3, 1, vec![]).unwrap();
        a.record_contribution(w(2), 2, 1, vec![]).unwrap();
        b.record_contribution(w(2), 2, 1, vec![]).unwrap();
        b.record_contribution(w(1), 5, 1, vec![]).unwrap();
        b.record_contribution(w(3), 3, 1, vec![]).unwrap();
        a.allocate_epoch(1).unwrap();
        b.allocate_epoch(1).unwrap();
        assert_eq!(a.iou_balance, b.iou_balance);
        assert_eq!(a.commons_balance, b.commons_balance);
    }

    #[test]
    fn delegate_conserves_and_is_gated() {
        let mut c = Commons::new();
        c.grant_citizen(w(1));
        c.grant_citizen(w(2));
        c.deposit(900, 1).unwrap();
        c.record_contribution(w(1), 1, 1, vec![]).unwrap();
        c.allocate_epoch(1).unwrap(); // all 900 → w1
        assert_eq!(*c.iou_balance.get(&w(1)).unwrap(), 900);
        // delegate to a non-citizen → rejected
        assert_eq!(c.delegate(w(1), w(9), 100, 2), Err(CommonsError::NotCitizen));
        // delegate to a citizen → conserves total
        c.delegate(w(1), w(2), 300, 2).unwrap();
        assert_eq!(*c.iou_balance.get(&w(1)).unwrap(), 600);
        assert_eq!(*c.iou_balance.get(&w(2)).unwrap(), 300);
        assert_eq!(c.outstanding_iou(), 900);
        assert_eq!(c.delegate(w(1), w(2), 10_000, 2), Err(CommonsError::InsufficientIou));
        c.check_invariants().unwrap();
    }

    #[test]
    fn redeem_reduces_and_cannot_over_redeem() {
        let mut c = Commons::new();
        c.grant_citizen(w(1));
        c.deposit(500, 1).unwrap();
        c.record_contribution(w(1), 1, 1, vec![]).unwrap();
        c.allocate_epoch(1).unwrap();
        c.redeem(w(1), 200, 2).unwrap();
        assert_eq!(*c.iou_balance.get(&w(1)).unwrap(), 300);
        assert_eq!(c.total_redeemed, 200);
        assert_eq!(c.redeem(w(1), 9999, 2), Err(CommonsError::InsufficientIou));
        c.check_invariants().unwrap();
    }

    #[test]
    fn full_lifecycle_holds_both_invariants() {
        let mut c = Commons::new();
        for i in 1..=3u8 {
            c.grant_citizen(w(i));
        }
        // three epochs of deposits + contributions
        for epoch in 1..=3u64 {
            c.deposit(777, epoch).unwrap();
            c.record_contribution(w(1), epoch as u128 * 2, epoch, vec![]).unwrap();
            c.record_contribution(w(2), 3, epoch, vec![]).unwrap();
            c.allocate_epoch(epoch).unwrap();
            c.check_invariants().unwrap();
        }
        c.delegate(w(1), w(3), 50, 4).unwrap();
        c.redeem(w(2), 100, 4).unwrap();
        c.redeem(w(3), 10, 4).unwrap();
        c.check_invariants().unwrap();
        // value is conserved: deposited == accrued + dust still in commons
        assert_eq!(c.total_deposited, c.total_accrued + c.commons_balance);
    }

    #[test]
    fn empty_epoch_keeps_pool_for_next_time() {
        let mut c = Commons::new();
        c.grant_citizen(w(1));
        c.deposit(1000, 1).unwrap();
        assert_eq!(c.allocate_epoch(1).unwrap(), 0); // no contributions
        assert_eq!(c.commons_balance, 1000); // pool intact
        c.record_contribution(w(1), 1, 2, vec![]).unwrap();
        assert_eq!(c.allocate_epoch(2).unwrap(), 1000); // now it all pays out
        c.check_invariants().unwrap();
    }

    #[test]
    fn serde_roundtrip() {
        let mut c = Commons::new();
        c.grant_citizen(w(1));
        c.deposit(100, 1).unwrap();
        c.record_contribution(w(1), 1, 1, vec!["full:deadbeef".into()]).unwrap();
        c.allocate_epoch(1).unwrap();
        let json = serde_json::to_string(&c).unwrap();
        let back: Commons = serde_json::from_str(&json).unwrap();
        assert_eq!(back.iou_balance, c.iou_balance);
        assert_eq!(back.events.len(), c.events.len());
        back.check_invariants().unwrap();
    }
}
