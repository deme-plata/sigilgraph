//! sigil-council — SIGIL Nations v0.2: fast-track governance.
//!
//! Two-speed council, the safe version of "ship fast":
//! - [`Risk::LowRisk`] (UI, docs, non-money dev) → **1-of-2** signer + simple majority. Fast.
//! - [`Risk::MoneyOrConsensus`] (treasury, emission, validation rules) → **2-of-2** signers + a **2/3
//!   franchise supermajority** + **quorum** (≥ half the franchise must vote). Strict.
//!
//! Votes are weighted by franchise (from sigil-citizenship tiers). When a proposal is finalized, its
//! outcome is folded into [`Council::gov_root`] — an incremental XOR accumulator over decided
//! proposals — so the governance record is O(1) per change, order-independent, and tamper-evident.
//! The only authority is the committed root; there is no drift-prone side table (the Quillon fix).

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Risk { LowRisk, MoneyOrConsensus }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome { Pending, Passed, Rejected }

#[derive(Clone, Debug)]
pub struct Proposal {
    pub id: u64,
    pub title: String,
    pub risk: Risk,
    pub for_w: u64,     // franchise weight voting FOR
    pub against_w: u64, // franchise weight voting AGAINST
    pub signers: u32,   // distinct council signers who endorsed
    pub outcome: Outcome,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error { NotFound, AlreadyDecided }

pub struct Council {
    total_franchise: u64,
    proposals: BTreeMap<u64, Proposal>,
    gov_root: [u8; 32],
}

fn proposal_hash(p: &Proposal) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&p.id.to_le_bytes());
    h.update(p.title.as_bytes());
    h.update(&[match p.risk { Risk::LowRisk => 1, Risk::MoneyOrConsensus => 2 }]);
    h.update(&p.for_w.to_le_bytes());
    h.update(&p.against_w.to_le_bytes());
    h.update(&[match p.outcome { Outcome::Passed => 1, Outcome::Rejected => 2, Outcome::Pending => 0 }]);
    *h.finalize().as_bytes()
}

fn xor_into(acc: &mut [u8; 32], h: &[u8; 32]) {
    for (a, b) in acc.iter_mut().zip(h.iter()) { *a ^= *b; }
}

impl Council {
    /// `total_franchise` is the sum of all citizens' franchise weight (from sigil-citizenship).
    pub fn new(total_franchise: u64) -> Self {
        Council { total_franchise, proposals: BTreeMap::new(), gov_root: [0u8; 32] }
    }

    /// Update the franchise base (quorum denominator) — the facade calls this as citizens are admitted.
    pub fn set_total_franchise(&mut self, total: u64) { self.total_franchise = total; }

    pub fn propose(&mut self, id: u64, title: impl Into<String>, risk: Risk) {
        self.proposals.insert(id, Proposal {
            id, title: title.into(), risk, for_w: 0, against_w: 0, signers: 0, outcome: Outcome::Pending,
        });
    }

    /// Cast a franchise-weighted vote.
    pub fn vote(&mut self, id: u64, weight: u64, support: bool) -> Result<(), Error> {
        let p = self.proposals.get_mut(&id).ok_or(Error::NotFound)?;
        if p.outcome != Outcome::Pending { return Err(Error::AlreadyDecided); }
        if support { p.for_w += weight; } else { p.against_w += weight; }
        Ok(())
    }

    /// A council member endorses (signs) the proposal.
    pub fn sign(&mut self, id: u64) -> Result<(), Error> {
        let p = self.proposals.get_mut(&id).ok_or(Error::NotFound)?;
        if p.outcome != Outcome::Pending { return Err(Error::AlreadyDecided); }
        p.signers += 1;
        Ok(())
    }

    /// Apply the risk-tiered threshold and commit the result into gov_root.
    pub fn finalize(&mut self, id: u64) -> Result<Outcome, Error> {
        let total = self.total_franchise;
        let p = self.proposals.get_mut(&id).ok_or(Error::NotFound)?;
        if p.outcome != Outcome::Pending { return Err(Error::AlreadyDecided); }
        let turnout = p.for_w + p.against_w;
        let passed = match p.risk {
            // low-risk: one signer + simple majority of cast votes
            Risk::LowRisk => p.signers >= 1 && p.for_w > p.against_w,
            // money/consensus: 2-of-2 signers + 2/3 supermajority + quorum (≥ half the franchise voted)
            Risk::MoneyOrConsensus => {
                p.signers >= 2
                    && turnout * 2 >= total            // quorum
                    && p.for_w * 3 >= turnout * 2      // ≥ 2/3 supermajority
            }
        };
        p.outcome = if passed { Outcome::Passed } else { Outcome::Rejected };
        let h = proposal_hash(p);
        xor_into(&mut self.gov_root, &h); // commit the decided proposal into the root
        Ok(p.outcome)
    }

    pub fn get(&self, id: u64) -> Option<&Proposal> { self.proposals.get(&id) }
    /// O(1) committed governance root over all DECIDED proposals (order-independent, tamper-evident).
    pub fn gov_root(&self) -> [u8; 32] { self.gov_root }
    pub fn gov_root_hex(&self) -> String { hex::encode(self.gov_root) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_risk_fasttracks_with_one_signer_majority() {
        let mut c = Council::new(100);
        c.propose(1, "ship the new cockpit panel", Risk::LowRisk);
        c.sign(1).unwrap();
        c.vote(1, 6, true).unwrap();
        c.vote(1, 2, false).unwrap();
        assert_eq!(c.finalize(1).unwrap(), Outcome::Passed);
    }

    #[test]
    fn low_risk_needs_a_signer() {
        let mut c = Council::new(100);
        c.propose(2, "tweak copy", Risk::LowRisk);
        c.vote(2, 9, true).unwrap(); // majority but NO signer
        assert_eq!(c.finalize(2).unwrap(), Outcome::Rejected);
    }

    #[test]
    fn money_needs_two_signers_supermajority_and_quorum() {
        let mut c = Council::new(100);
        c.propose(3, "move 5000 from treasury", Risk::MoneyOrConsensus);
        c.sign(3).unwrap(); c.sign(3).unwrap();      // 2-of-2
        c.vote(3, 70, true).unwrap();                 // turnout 80 ≥ quorum 50; 70/80 ≥ 2/3
        c.vote(3, 10, false).unwrap();
        assert_eq!(c.finalize(3).unwrap(), Outcome::Passed);
    }

    #[test]
    fn money_rejected_below_quorum() {
        let mut c = Council::new(100);
        c.propose(4, "drain treasury quietly", Risk::MoneyOrConsensus);
        c.sign(4).unwrap(); c.sign(4).unwrap();
        c.vote(4, 30, true).unwrap();   // turnout 30 < quorum 50
        assert_eq!(c.finalize(4).unwrap(), Outcome::Rejected);
    }

    #[test]
    fn money_rejected_without_two_signers() {
        let mut c = Council::new(100);
        c.propose(5, "change emission", Risk::MoneyOrConsensus);
        c.sign(5).unwrap();             // only 1-of-2
        c.vote(5, 90, true).unwrap();
        assert_eq!(c.finalize(5).unwrap(), Outcome::Rejected);
    }

    #[test]
    fn gov_root_moves_on_decision_and_is_order_independent() {
        let mut a = Council::new(100);
        let mut b = Council::new(100);
        let r0 = a.gov_root();
        // same two decisions, applied in OPPOSITE order to a vs b
        a.propose(1, "p1", Risk::LowRisk); a.sign(1).unwrap(); a.vote(1, 5, true).unwrap(); a.finalize(1).unwrap();
        a.propose(2, "p2", Risk::LowRisk); a.sign(2).unwrap(); a.vote(2, 5, true).unwrap(); a.finalize(2).unwrap();
        b.propose(2, "p2", Risk::LowRisk); b.sign(2).unwrap(); b.vote(2, 5, true).unwrap(); b.finalize(2).unwrap();
        b.propose(1, "p1", Risk::LowRisk); b.sign(1).unwrap(); b.vote(1, 5, true).unwrap(); b.finalize(1).unwrap();
        assert_ne!(a.gov_root(), r0);             // root moved
        assert_eq!(a.gov_root(), b.gov_root());   // committed set, not sequence
    }

    #[test]
    fn cannot_double_finalize() {
        let mut c = Council::new(100);
        c.propose(7, "x", Risk::LowRisk); c.sign(7).unwrap(); c.vote(7, 1, true).unwrap();
        c.finalize(7).unwrap();
        assert_eq!(c.finalize(7), Err(Error::AlreadyDecided));
    }
}
