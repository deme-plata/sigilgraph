//! sigil-ballot — SIGIL Nations v0.3: the integrity-checked, committed vote record.
//!
//! [`sigil-council`](../sigil_council) decides the *thresholds*; this crate is **who voted**. It
//! enforces **one vote per citizen per proposal** (no double-vote / ballot-stuffing) and folds every
//! accepted vote into [`Ballot::root`] — an incremental XOR accumulator — so the poll is O(1) per
//! vote, order-independent, and tamper-evident. The franchise-weighted [`Ballot::tally`] is what you
//! hand to the council to finalize. A raw running total can't catch a citizen voting twice; this can.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, PartialEq, Eq)]
pub enum Error { AlreadyVoted }

#[derive(Default)]
pub struct Ballot {
    cast: BTreeSet<(u64, String)>,    // (proposal, citizen) already counted
    tally: BTreeMap<u64, (u64, u64)>, // proposal → (for_weight, against_weight)
    voters: BTreeMap<u64, u32>,       // proposal → distinct voter count
    root: [u8; 32],
}

fn vote_hash(proposal: u64, citizen: &str, weight: u64, support: bool) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&proposal.to_le_bytes());
    h.update(citizen.as_bytes());
    h.update(&weight.to_le_bytes());
    h.update(&[support as u8]);
    *h.finalize().as_bytes()
}

fn xor_into(acc: &mut [u8; 32], h: &[u8; 32]) {
    for (a, b) in acc.iter_mut().zip(h.iter()) { *a ^= *b; }
}

impl Ballot {
    pub fn new() -> Self { Self::default() }

    /// Record a franchise-weighted vote. Rejects a second vote by the same citizen on the same
    /// proposal, and commits the accepted vote into the root in O(1).
    pub fn cast_vote(&mut self, proposal: u64, citizen: &str, weight: u64, support: bool) -> Result<(), Error> {
        let key = (proposal, citizen.to_string());
        if self.cast.contains(&key) { return Err(Error::AlreadyVoted); }
        self.cast.insert(key);
        let e = self.tally.entry(proposal).or_insert((0, 0));
        if support { e.0 += weight; } else { e.1 += weight; }
        *self.voters.entry(proposal).or_insert(0) += 1;
        xor_into(&mut self.root, &vote_hash(proposal, citizen, weight, support));
        Ok(())
    }

    /// (for_weight, against_weight) — hand this to sigil-council to finalize.
    pub fn tally(&self, proposal: u64) -> (u64, u64) { self.tally.get(&proposal).copied().unwrap_or((0, 0)) }
    pub fn voters(&self, proposal: u64) -> u32 { self.voters.get(&proposal).copied().unwrap_or(0) }
    pub fn has_voted(&self, proposal: u64, citizen: &str) -> bool { self.cast.contains(&(proposal, citizen.to_string())) }

    /// O(1) committed root over every accepted vote (order-independent, tamper-evident).
    pub fn root(&self) -> [u8; 32] { self.root }
    pub fn root_hex(&self) -> String { hex::encode(self.root) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_and_tally() {
        let mut b = Ballot::new();
        b.cast_vote(1, "qnk_a", 4, true).unwrap();
        b.cast_vote(1, "qnk_b", 2, false).unwrap();
        b.cast_vote(1, "qnk_c", 1, true).unwrap();
        assert_eq!(b.tally(1), (5, 2));
        assert_eq!(b.voters(1), 3);
    }

    #[test]
    fn double_vote_rejected() {
        let mut b = Ballot::new();
        b.cast_vote(1, "qnk_a", 4, true).unwrap();
        assert_eq!(b.cast_vote(1, "qnk_a", 9, false), Err(Error::AlreadyVoted));
        assert_eq!(b.tally(1), (4, 0)); // second vote did NOT count
        assert_eq!(b.voters(1), 1);
    }

    #[test]
    fn same_citizen_may_vote_on_different_proposals() {
        let mut b = Ballot::new();
        b.cast_vote(1, "qnk_a", 4, true).unwrap();
        b.cast_vote(2, "qnk_a", 4, false).unwrap();
        assert_eq!(b.tally(1), (4, 0));
        assert_eq!(b.tally(2), (0, 4));
        assert!(b.has_voted(1, "qnk_a") && b.has_voted(2, "qnk_a"));
    }

    #[test]
    fn empty_root_is_zero_and_moves_on_vote() {
        let mut b = Ballot::new();
        assert_eq!(b.root(), [0u8; 32]);
        b.cast_vote(1, "qnk_a", 1, true).unwrap();
        assert_ne!(b.root(), [0u8; 32]);
    }

    #[test]
    fn root_is_order_independent() {
        let mut a = Ballot::new();
        let mut b = Ballot::new();
        a.cast_vote(1, "qnk_a", 4, true).unwrap();
        a.cast_vote(1, "qnk_b", 2, false).unwrap();
        b.cast_vote(1, "qnk_b", 2, false).unwrap();
        b.cast_vote(1, "qnk_a", 4, true).unwrap();
        assert_eq!(a.root(), b.root()); // committed set of votes, not sequence
    }

    #[test]
    fn unknown_proposal_is_empty() {
        let b = Ballot::new();
        assert_eq!(b.tally(99), (0, 0));
        assert_eq!(b.voters(99), 0);
        assert!(!b.has_voted(99, "qnk_x"));
    }
}
