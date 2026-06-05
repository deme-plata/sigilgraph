//! sigil-citizenship — SIGIL Nations v0.1: the on-chain citizen registry.
//!
//! A citizen is a settlement address (`qnk…`) bound to a [`CitizenId`] and a
//! franchise [`Tier`], admitted only after an [`Attestor`] verifies an
//! attestation. The attestor is a trait, not a hardwired scheme, so the
//! signature primitive (SQIsign today) can be swapped without touching the
//! registry — crypto-agility per the Stargate discipline.
//!
//! The set of citizens is committed in an **incremental XOR multiset
//! accumulator**: [`Registry::citizenship_root`] is O(1) per change,
//! order-independent, and tamper-evident. Admitting/revoking just XORs the
//! entry hash in/out. This is the Quillon-postmortem rule — the only authority
//! is the committed root, never a drift-prone side table.

use std::collections::BTreeMap;

/// Verifies that `attestation` authorizes `addr` to be admitted. Implementations
/// plug in the real signature scheme (SQIsign / flux-eternal-cypher dispatch).
pub trait Attestor {
    fn verify(&self, addr: &str, attestation: &[u8]) -> bool;
}

/// Accepts any non-empty attestation. For tests / bootstrap only — production
/// uses a SQIsign-class attestor.
pub struct AcceptNonEmpty;
impl Attestor for AcceptNonEmpty {
    fn verify(&self, _addr: &str, attestation: &[u8]) -> bool { !attestation.is_empty() }
}

/// Franchise weight — ties into sigil-university credentials (feature v0.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier { Bronze, Silver, Gold }
impl Tier {
    /// Voting weight this tier carries in sigil-ballot (feature v0.3).
    pub fn franchise(self) -> u32 { match self { Tier::Bronze => 1, Tier::Silver => 2, Tier::Gold => 4 } }
    fn tag(self) -> u8 { match self { Tier::Bronze => 1, Tier::Silver => 2, Tier::Gold => 3 } }
}

/// Deterministic 16-byte citizen id = BLAKE3(addr)[..16].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CitizenId(pub [u8; 16]);
impl CitizenId {
    fn of(addr: &str) -> Self {
        let h = blake3::hash(addr.as_bytes());
        let mut id = [0u8; 16];
        id.copy_from_slice(&h.as_bytes()[..16]);
        CitizenId(id)
    }
    pub fn hex(&self) -> String { hex::encode(self.0) }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Citizen {
    pub addr: String,
    pub id: CitizenId,
    pub tier: Tier,
    pub admitted_height: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    AlreadyCitizen,
    NotCitizen,
    BadAttestation,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::AlreadyCitizen => write!(f, "address is already a citizen"),
            Error::NotCitizen => write!(f, "address is not a citizen"),
            Error::BadAttestation => write!(f, "attestation failed verification"),
        }
    }
}
impl std::error::Error for Error {}

/// The committed citizen registry.
#[derive(Default)]
pub struct Registry {
    citizens: BTreeMap<String, Citizen>,
    acc: [u8; 32], // XOR multiset accumulator over per-citizen hashes
}

fn entry_hash(c: &Citizen) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(c.addr.as_bytes());
    h.update(&c.id.0);
    h.update(&[c.tier.tag()]);
    h.update(&c.admitted_height.to_le_bytes());
    *h.finalize().as_bytes()
}

fn xor_into(acc: &mut [u8; 32], h: &[u8; 32]) {
    for (a, b) in acc.iter_mut().zip(h.iter()) { *a ^= *b; }
}

impl Registry {
    pub fn new() -> Self { Self::default() }

    /// Admit `addr` after the attestor verifies `attestation`. Commits the new
    /// citizen into the root in O(1).
    pub fn admit<A: Attestor>(&mut self, addr: &str, tier: Tier, attestation: &[u8], height: u64, attestor: &A) -> Result<CitizenId, Error> {
        if self.citizens.contains_key(addr) { return Err(Error::AlreadyCitizen); }
        if !attestor.verify(addr, attestation) { return Err(Error::BadAttestation); }
        let c = Citizen { addr: addr.to_string(), id: CitizenId::of(addr), tier, admitted_height: height };
        xor_into(&mut self.acc, &entry_hash(&c));
        let id = c.id;
        self.citizens.insert(addr.to_string(), c);
        Ok(id)
    }

    /// Revoke a citizen, XORing them back out of the root (exact inverse of admit).
    pub fn revoke(&mut self, addr: &str) -> Result<(), Error> {
        let c = self.citizens.remove(addr).ok_or(Error::NotCitizen)?;
        xor_into(&mut self.acc, &entry_hash(&c));
        Ok(())
    }

    pub fn is_citizen(&self, addr: &str) -> bool { self.citizens.contains_key(addr) }
    pub fn get(&self, addr: &str) -> Option<&Citizen> { self.citizens.get(addr) }
    pub fn count(&self) -> usize { self.citizens.len() }

    /// Total franchise weight across all citizens (for sigil-ballot quorum math).
    pub fn total_franchise(&self) -> u64 { self.citizens.values().map(|c| c.tier.franchise() as u64).sum() }

    /// O(1) committed root over the citizen set. Order-independent and
    /// tamper-evident: identical sets → identical root regardless of admit order.
    pub fn citizenship_root(&self) -> [u8; 32] { self.acc }
    pub fn root_hex(&self) -> String { hex::encode(self.acc) }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &AcceptNonEmpty = &AcceptNonEmpty;

    #[test]
    fn admit_then_lookup() {
        let mut r = Registry::new();
        let id = r.admit("qnk_alice", Tier::Gold, b"att", 10, A).unwrap();
        assert!(r.is_citizen("qnk_alice"));
        assert_eq!(r.get("qnk_alice").unwrap().id, id);
        assert_eq!(r.count(), 1);
        assert_eq!(r.total_franchise(), 4);
    }

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(Registry::new().citizenship_root(), [0u8; 32]);
    }

    #[test]
    fn root_changes_on_admit_and_inverts_on_revoke() {
        let mut r = Registry::new();
        let r0 = r.citizenship_root();
        r.admit("qnk_bob", Tier::Bronze, b"att", 5, A).unwrap();
        let r1 = r.citizenship_root();
        assert_ne!(r0, r1);
        r.revoke("qnk_bob").unwrap();
        assert_eq!(r.citizenship_root(), r0); // exact XOR inverse
        assert_eq!(r.count(), 0);
    }

    #[test]
    fn duplicate_admit_rejected() {
        let mut r = Registry::new();
        r.admit("qnk_a", Tier::Silver, b"att", 1, A).unwrap();
        assert_eq!(r.admit("qnk_a", Tier::Silver, b"att", 1, A), Err(Error::AlreadyCitizen));
    }

    #[test]
    fn empty_attestation_rejected() {
        let mut r = Registry::new();
        assert_eq!(r.admit("qnk_a", Tier::Bronze, b"", 1, A), Err(Error::BadAttestation));
        assert!(!r.is_citizen("qnk_a"));
    }

    #[test]
    fn revoke_unknown_rejected() {
        assert_eq!(Registry::new().revoke("qnk_ghost"), Err(Error::NotCitizen));
    }

    #[test]
    fn root_is_order_independent() {
        let mut a = Registry::new();
        let mut b = Registry::new();
        a.admit("qnk_1", Tier::Gold, b"x", 1, A).unwrap();
        a.admit("qnk_2", Tier::Bronze, b"x", 2, A).unwrap();
        b.admit("qnk_2", Tier::Bronze, b"x", 2, A).unwrap();
        b.admit("qnk_1", Tier::Gold, b"x", 1, A).unwrap();
        assert_eq!(a.citizenship_root(), b.citizenship_root()); // committed set, not sequence
    }

    #[test]
    fn id_is_deterministic() {
        assert_eq!(CitizenId::of("qnk_x"), CitizenId::of("qnk_x"));
        assert_ne!(CitizenId::of("qnk_x"), CitizenId::of("qnk_y"));
    }
}
