//! # flux-pool — the provable mining pool.
//!
//! Miners submit **shares** (weighted by the difficulty they solved). When the
//! pool earns a BTC reward (e.g. a pool block, or accumulated LN micro-income),
//! it splits it **proportionally to committed shares** and pays each miner a
//! small Lightning amount. The difference from a normal pool: the share set is
//! committed in **one constant-size flux-fold attestation**, and every payout is
//! an independently recomputable function of the published shares — so a miner
//! can *prove* their cut was fair instead of trusting the operator.
//!
//! ```text
//!   flux-miner shares ─▶ ShareLedger ─▶ payouts() (proportional, LN)
//!                              └─▶ attest() ─▶ one flux-fold proof (constant size)
//!                                              every miner verifies their share is in it
//! ```

use flux_fold::{fold, Ajtai, FoldedProof, Q};
use std::collections::BTreeMap;

/// Public seed for the pool's transparent Ajtai matrix (anyone regenerates it).
const POOL_SEED: [u8; 32] = *b"flux-pool/ajtai/v1//////////////";
const AJTAI_M: usize = 4;
const AJTAI_N: usize = 8;

/// One miner's accumulated share weight (sum of solved difficulties).
#[derive(Debug, Clone, Default)]
pub struct ShareLedger {
    weights: BTreeMap<String, u128>,
    total_weight: u128,
}

/// A computed payout for one miner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payout {
    pub miner: String,
    pub sats: u128,
    pub weight: u128,
}

impl ShareLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a submitted, accepted share of the given difficulty `weight`.
    pub fn record_share(&mut self, miner: impl Into<String>, weight: u128) {
        let m = miner.into();
        *self.weights.entry(m).or_insert(0) += weight;
        self.total_weight += weight;
    }

    pub fn total_weight(&self) -> u128 {
        self.total_weight
    }
    pub fn miners(&self) -> usize {
        self.weights.len()
    }
    pub fn weight_of(&self, miner: &str) -> u128 {
        self.weights.get(miner).copied().unwrap_or(0)
    }

    /// Split `reward_sat` proportionally to share weight (floor division; the
    /// dust remainder stays in the pool for the next round). Deterministic +
    /// independently recomputable — that's the "provable" part.
    pub fn payouts(&self, reward_sat: u128) -> Vec<Payout> {
        if self.total_weight == 0 {
            return Vec::new();
        }
        self.weights
            .iter()
            .map(|(miner, &weight)| Payout {
                miner: miner.clone(),
                sats: reward_sat.saturating_mul(weight) / self.total_weight,
                weight,
            })
            .collect()
    }

    /// Independently verify a single miner's payout (what a miner runs to check
    /// the operator didn't cheat): recompute floor(reward·weight/total).
    pub fn verify_payout(&self, miner: &str, claimed_sats: u128, reward_sat: u128) -> bool {
        if self.total_weight == 0 {
            return claimed_sats == 0;
        }
        let owed = reward_sat.saturating_mul(self.weight_of(miner)) / self.total_weight;
        owed == claimed_sats
    }

    /// Encode one (miner, weight) share as a flux-fold witness vector (len n).
    fn witness(miner: &str, weight: u128) -> Vec<u64> {
        let h = blake3::hash(miner.as_bytes());
        let b = h.as_bytes();
        let mut w = vec![0u64; AJTAI_N];
        w[0] = (weight % Q as u128) as u64;
        for i in 0..(AJTAI_N - 1) {
            let chunk = u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
            w[i + 1] = chunk % Q;
        }
        w
    }

    /// Commit the WHOLE share set into ONE constant-size flux-fold proof. Its
    /// size is independent of how many miners are in the pool — so the pool can
    /// publish a fixed-size "these are all the shares this round" attestation.
    pub fn attest(&self) -> FoldedProof {
        let ajtai = Ajtai::from_seed(AJTAI_M, AJTAI_N, &POOL_SEED);
        let witnesses: Vec<Vec<u64>> = self.weights.iter().map(|(m, &w)| Self::witness(m, w)).collect();
        fold(&ajtai, &witnesses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> ShareLedger {
        let mut l = ShareLedger::new();
        l.record_share("alice", 60);
        l.record_share("bob", 30);
        l.record_share("carol", 10); // total 100
        l
    }

    #[test]
    fn payouts_are_proportional_and_bounded() {
        let l = pool();
        let reward = 1_000_000u128; // 1M sat
        let ps = l.payouts(reward);
        let by = |m: &str| ps.iter().find(|p| p.miner == m).unwrap().sats;
        assert_eq!(by("alice"), 600_000);
        assert_eq!(by("bob"), 300_000);
        assert_eq!(by("carol"), 100_000);
        // never pay out more than the reward.
        let sum: u128 = ps.iter().map(|p| p.sats).sum();
        assert!(sum <= reward);
    }

    #[test]
    fn miner_can_independently_verify_their_cut() {
        let l = pool();
        let reward = 777_777u128;
        // honest claim verifies; a cheated (inflated) claim fails.
        let owed = reward * 60 / 100;
        assert!(l.verify_payout("alice", owed, reward));
        assert!(!l.verify_payout("alice", owed + 1, reward));
    }

    #[test]
    fn attestation_is_constant_size_regardless_of_miner_count() {
        let small = pool().attest().size_bytes();
        let mut big = ShareLedger::new();
        for i in 0..500 {
            big.record_share(format!("miner{i}"), (i as u128) + 1);
        }
        assert_eq!(small, big.attest().size_bytes(), "fold attestation must be constant-size in miner count");
    }

    #[test]
    fn changing_a_share_changes_the_attestation() {
        let a = pool().attest();
        let mut l2 = pool();
        l2.record_share("alice", 1); // bump alice
        let b = l2.attest();
        assert_ne!((a.c_star, a.w_star), (b.c_star, b.w_star), "tampering the share set must move the attestation");
    }
}
