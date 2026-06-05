//! sigil-nation — NATION-IN-A-BOX, the facade.
//!
//! One [`Nation`] composes the four primitives — [`sigil_citizenship`], [`sigil_ballot`],
//! [`sigil_council`], [`sigil_treasury`] — into a single sovereign agent economy with one committed
//! [`Nation::nation_root`]. The full loop in one object:
//!
//! ```text
//! admit → propose → endorse(×signers) → vote(per citizen) → finalize → pay
//! ```
//!
//! Every step is committed in a sub-root, and `nation_root` is the BLAKE3 of all four — so the whole
//! economy's state lives in one root. Voting is citizen-only and one-per-citizen (ballot), money moves
//! only on a council 2-of-2 (council + treasury), and the treasury balance is max-wins (Quillon fix).

pub use sigil_citizenship::{Attestor, AcceptNonEmpty, CitizenId, Tier};
pub use sigil_council::{Outcome, Risk};

use sigil_ballot::Ballot;
use sigil_citizenship::Registry;
use sigil_council::Council;
use sigil_treasury::Treasury;

#[derive(Debug, PartialEq, Eq)]
pub enum NationError {
    NotCitizen,
    AlreadyCitizen,
    BadAttestation,
    AlreadyVoted,
    BadProposal,
    NotApproved,
    Insufficient,
    AlreadySpent,
}

pub struct Nation {
    citizens: Registry,
    ballot: Ballot,
    council: Council,
    treasury: Treasury,
}

impl Default for Nation {
    fn default() -> Self { Self::new() }
}

impl Nation {
    pub fn new() -> Self {
        Nation { citizens: Registry::new(), ballot: Ballot::new(), council: Council::new(0), treasury: Treasury::new() }
    }

    /// Admit a citizen (gated by the attestor). Updates the council's franchise base.
    pub fn admit<A: Attestor>(&mut self, addr: &str, tier: Tier, attestation: &[u8], height: u64, attestor: &A) -> Result<CitizenId, NationError> {
        let id = self.citizens.admit(addr, tier, attestation, height, attestor).map_err(|e| match e {
            sigil_citizenship::Error::AlreadyCitizen => NationError::AlreadyCitizen,
            sigil_citizenship::Error::BadAttestation => NationError::BadAttestation,
            sigil_citizenship::Error::NotCitizen => NationError::NotCitizen,
        })?;
        self.council.set_total_franchise(self.citizens.total_franchise());
        Ok(id)
    }

    pub fn propose(&mut self, id: u64, title: impl Into<String>, risk: Risk) {
        self.council.propose(id, title, risk);
    }

    /// A council member endorses (signs) a proposal.
    pub fn endorse(&mut self, proposal: u64) -> Result<(), NationError> {
        self.council.sign(proposal).map_err(|_| NationError::BadProposal)
    }

    /// A citizen casts a franchise-weighted vote (one per citizen per proposal).
    pub fn vote(&mut self, proposal: u64, citizen: &str, support: bool) -> Result<(), NationError> {
        let c = self.citizens.get(citizen).ok_or(NationError::NotCitizen)?;
        let weight = c.tier.franchise() as u64;
        self.ballot.cast_vote(proposal, citizen, weight, support).map_err(|_| NationError::AlreadyVoted)
    }

    /// Transfer the ballot's integrity-checked tally into the council and apply the risk-tiered
    /// threshold, committing the outcome.
    pub fn finalize(&mut self, proposal: u64) -> Result<Outcome, NationError> {
        let (for_w, against_w) = self.ballot.tally(proposal);
        self.council.set_total_franchise(self.citizens.total_franchise());
        if for_w > 0 { self.council.vote(proposal, for_w, true).map_err(|_| NationError::BadProposal)?; }
        if against_w > 0 { self.council.vote(proposal, against_w, false).map_err(|_| NationError::BadProposal)?; }
        self.council.finalize(proposal).map_err(|_| NationError::BadProposal)
    }

    /// Collect the 10% dev fee on settled work into the treasury.
    pub fn collect_fee(&mut self, gross: u128) -> u128 { self.treasury.collect_dev_fee(gross) }
    /// Direct endowment into the treasury.
    pub fn endow(&mut self, amount: u128) { self.treasury.accrue(amount); }

    /// Pay out of the treasury — allowed ONLY if `proposal` is a passed MoneyOrConsensus vote (2-of-2).
    pub fn pay(&mut self, amount: u128, proposal: u64) -> Result<(), NationError> {
        let p = self.council.get(proposal).ok_or(NationError::BadProposal)?;
        let approved = p.outcome == Outcome::Passed && p.risk == Risk::MoneyOrConsensus;
        self.treasury.payout(proposal, amount, approved).map_err(|e| match e {
            sigil_treasury::Error::NotApproved => NationError::NotApproved,
            sigil_treasury::Error::Insufficient => NationError::Insufficient,
            sigil_treasury::Error::AlreadySpent => NationError::AlreadySpent,
        })
    }

    pub fn citizen_count(&self) -> usize { self.citizens.count() }
    pub fn treasury_balance(&self) -> u128 { self.treasury.balance() }
    pub fn total_franchise(&self) -> u64 { self.citizens.total_franchise() }
    pub fn outcome(&self, proposal: u64) -> Option<Outcome> { self.council.get(proposal).map(|p| p.outcome) }

    /// The ONE committed root over the whole nation: citizenship ⊕ ballot ⊕ council ⊕ treasury.
    pub fn nation_root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(&self.citizens.citizenship_root());
        h.update(&self.ballot.root());
        h.update(&self.council.gov_root());
        h.update(&self.treasury.root());
        *h.finalize().as_bytes()
    }
    pub fn nation_root_hex(&self) -> String { hex::encode(self.nation_root()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &AcceptNonEmpty = &AcceptNonEmpty;

    #[test]
    fn spin_up_a_sovereign_economy() {
        let mut n = Nation::new();
        // 3 citizens (franchise 4 + 2 + 1 = 7)
        n.admit("qnk_a", Tier::Gold, b"att", 1, A).unwrap();
        n.admit("qnk_b", Tier::Silver, b"att", 1, A).unwrap();
        n.admit("qnk_c", Tier::Bronze, b"att", 1, A).unwrap();
        n.collect_fee(10_000); // 10% → 1000 into treasury
        // a treasury grant, the strict path
        n.propose(1, "grant 500 to research", Risk::MoneyOrConsensus);
        n.endorse(1).unwrap();
        n.endorse(1).unwrap(); // 2-of-2 signers
        n.vote(1, "qnk_a", true).unwrap(); // 4 for
        n.vote(1, "qnk_b", true).unwrap(); // 2 for → turnout 6/7 (quorum), 6/6 ≥ 2/3
        assert_eq!(n.finalize(1).unwrap(), Outcome::Passed);
        n.pay(500, 1).unwrap();
        assert_eq!(n.treasury_balance(), 500);
        assert_eq!(n.citizen_count(), 3);
        assert_eq!(n.total_franchise(), 7);
        assert_ne!(n.nation_root(), [0u8; 32]);
    }

    #[test]
    fn non_citizen_cannot_vote() {
        let mut n = Nation::new();
        n.propose(1, "x", Risk::LowRisk);
        assert_eq!(n.vote(1, "qnk_stranger", true), Err(NationError::NotCitizen));
    }

    #[test]
    fn citizen_cannot_vote_twice() {
        let mut n = Nation::new();
        n.admit("qnk_a", Tier::Gold, b"att", 1, A).unwrap();
        n.propose(1, "x", Risk::LowRisk);
        n.vote(1, "qnk_a", true).unwrap();
        assert_eq!(n.vote(1, "qnk_a", false), Err(NationError::AlreadyVoted));
    }

    #[test]
    fn cannot_pay_without_a_passed_money_proposal() {
        let mut n = Nation::new();
        n.admit("qnk_a", Tier::Gold, b"att", 1, A).unwrap();
        n.endow(1000);
        n.propose(1, "low risk thing", Risk::LowRisk);
        n.endorse(1).unwrap();
        n.vote(1, "qnk_a", true).unwrap();
        n.finalize(1).unwrap(); // passes, but it's LowRisk not a money proposal
        assert_eq!(n.pay(100, 1), Err(NationError::NotApproved));
        assert_eq!(n.treasury_balance(), 1000); // nothing moved
    }

    #[test]
    fn money_proposal_below_quorum_blocks_the_payout() {
        let mut n = Nation::new();
        for a in ["qnk_a", "qnk_b", "qnk_c", "qnk_d"] { n.admit(a, Tier::Gold, b"att", 1, A).unwrap(); } // franchise 16
        n.endow(1000);
        n.propose(1, "quiet drain", Risk::MoneyOrConsensus);
        n.endorse(1).unwrap();
        n.endorse(1).unwrap();
        n.vote(1, "qnk_a", true).unwrap(); // turnout 4 of 16 → below quorum
        assert_eq!(n.finalize(1).unwrap(), Outcome::Rejected);
        assert_eq!(n.pay(100, 1), Err(NationError::NotApproved));
    }

    #[test]
    fn nation_enforces_spend_once() {
        let mut n = Nation::new();
        for c in ["qnk_a", "qnk_b", "qnk_c"] { n.admit(c, Tier::Gold, b"att", 1, A).unwrap(); } // franchise 12
        n.endow(1000);
        n.propose(1, "grant", Risk::MoneyOrConsensus);
        n.endorse(1).unwrap();
        n.endorse(1).unwrap();
        n.vote(1, "qnk_a", true).unwrap();
        n.vote(1, "qnk_b", true).unwrap(); // turnout 8/12, 8/8 ≥ 2/3
        assert_eq!(n.finalize(1).unwrap(), Outcome::Passed);
        n.pay(300, 1).unwrap();
        assert_eq!(n.pay(300, 1), Err(NationError::AlreadySpent)); // can't draw the same proposal twice
        assert_eq!(n.treasury_balance(), 700);
    }

    #[test]
    fn nation_root_changes_as_the_economy_evolves() {
        let mut n = Nation::new();
        let r0 = n.nation_root();
        n.admit("qnk_a", Tier::Gold, b"att", 1, A).unwrap();
        let r1 = n.nation_root();
        assert_ne!(r0, r1);
        n.collect_fee(1000);
        assert_ne!(r1, n.nation_root());
    }
}
