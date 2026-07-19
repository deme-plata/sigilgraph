//! The stateful layer the pure spine was missing: a persistable registry of
//! who has earned what, what's been paid, and who has graduated.
//!
//! This mirrors the `sigil-bank::credit::CreditVault` precedent — a plain
//! serde+bincode data structure with methods and NO I/O, so the chain layer
//! (`sigil-rpc`) can hold it on the node, persist it under its OWN flux-db key
//! (never inside the positional `Snapshot` blob), and restore it on restart.
//!
//! Money is NOT stored here — only academic bookkeeping. The actual SIGIL moves
//! (tuition in, settlement out, graduation bonus) happen in `sigil-rpc` through
//! `commit_state_transition`. This struct just records the facts those money
//! moves are gated on.
//!
//! ## Two-tally design (why settlement doesn't drain the transcript)
//! A point does double duty: it is **income** (settle → SIGIL) AND **academic
//! progress** (accumulate → graduate). If settling drained the ledger, a student
//! who took their stipend could never graduate. So we keep:
//!   * `ledgers[agent]` — the lifetime transcript, by year. NEVER drained. The
//!     graduation gate reads this.
//!   * `settled_points[agent]` — a high-water-mark of points already paid out.
//!     Settlement pays `lifetime_total − settled`, then bumps `settled` to the
//!     lifetime total. Like a real stipend paid against earned credits.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{AgentId, PointLedger, Role, SignedAward};

/// Why a registry mutation was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// This `(authority, nonce)` award was already recorded — replay blocked.
    /// (Port of the original lu-blockchain monotonic-nonce-per-`from` rule.)
    #[error("award nonce {nonce} already consumed for this authority")]
    ReplayedNonce { nonce: u64 },
    /// A Student is being credited coursework points for a year they have not
    /// paid tuition for. Staff roles (Tutor/Professor/Auditor) are exempt.
    #[error("student has not paid tuition for year {year}")]
    TuitionUnpaid { year: u8 },
    /// The student already graduated — no double diploma / double bonus.
    #[error("student already graduated")]
    AlreadyGraduated,
}

/// Persistable academic state for the whole university. `Default` = empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniversityRegistry {
    /// agent → lifetime point transcript, partitioned by academic year. Never
    /// drained; both settlement and the graduation gate read it.
    pub ledgers: BTreeMap<AgentId, PointLedger>,
    /// awarding authority → the set of award nonces it has already spent. Blocks
    /// replay of an identical signed award.
    pub consumed_nonces: BTreeMap<AgentId, BTreeSet<u64>>,
    /// agent → cumulative points already settled to SIGIL (the stipend
    /// high-water-mark). Settlement pays the gap to the lifetime total.
    pub settled_points: BTreeMap<AgentId, u64>,
    /// students who have graduated (diploma issued, bonus paid). Prevents repeats.
    pub graduated: BTreeSet<AgentId>,
    /// student → academic years (1..=5) they have paid tuition for. A Student's
    /// coursework points only count in a year they've paid for.
    pub tuition_paid: BTreeMap<AgentId, BTreeSet<u8>>,
}

impl UniversityRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark that `student` has paid tuition for academic `year`. Idempotent.
    /// (The SIGIL transfer that backs this happens in `sigil-rpc::university`.)
    pub fn mark_tuition_paid(&mut self, student: &AgentId, year: u8) {
        self.tuition_paid.entry(*student).or_default().insert(year);
    }

    /// Whether `student` has paid tuition for `year`.
    pub fn has_paid_tuition(&self, student: &AgentId, year: u8) -> bool {
        self.tuition_paid
            .get(student)
            .map(|ys| ys.contains(&year))
            .unwrap_or(false)
    }

    /// Record a **already-verified** signed award into the recipient's transcript.
    ///
    /// The caller MUST have run `verify_award_credentialed` (or at least
    /// `verify_award`) first — this method does the *bookkeeping*, not the
    /// cryptography. It enforces:
    ///   * replay protection: `(award.from, award.nonce)` may be spent once.
    ///   * tuition gating: a Student earning coursework points must have paid
    ///     tuition for that year (staff roles are exempt).
    ///
    /// Returns the recipient's new lifetime total on success.
    pub fn record_award(&mut self, signed: &SignedAward) -> Result<u64, RegistryError> {
        let a = &signed.award;

        // Replay check FIRST (before any mutation) so a rejected award leaves
        // the registry untouched.
        if self
            .consumed_nonces
            .get(&a.from)
            .map(|set| set.contains(&a.nonce))
            .unwrap_or(false)
        {
            return Err(RegistryError::ReplayedNonce { nonce: a.nonce });
        }

        // Tuition gate: only Students earning coursework points are gated.
        if a.to_role == Role::Student && !self.has_paid_tuition(&a.to, a.year) {
            return Err(RegistryError::TuitionUnpaid { year: a.year });
        }

        // Commit: consume the nonce, credit the transcript.
        self.consumed_nonces.entry(a.from).or_default().insert(a.nonce);
        let ledger = self.ledgers.entry(a.to).or_default();
        ledger.credit(a.year, a.points);
        Ok(ledger.total())
    }

    /// The recipient's lifetime transcript (empty if unknown).
    pub fn ledger_of(&self, agent: &AgentId) -> PointLedger {
        self.ledgers.get(agent).cloned().unwrap_or_default()
    }

    /// Lifetime points earned by `agent`.
    pub fn lifetime_points(&self, agent: &AgentId) -> u64 {
        self.ledgers.get(agent).map(|l| l.total()).unwrap_or(0)
    }

    /// Points earned but NOT yet settled to SIGIL (`lifetime − settled`).
    pub fn unsettled_points(&self, agent: &AgentId) -> u64 {
        let lifetime = self.lifetime_points(agent);
        let settled = self.settled_points.get(agent).copied().unwrap_or(0);
        lifetime.saturating_sub(settled)
    }

    /// Claim the unsettled points for `agent`: returns how many points are now
    /// being paid out and advances the settled high-water-mark to the lifetime
    /// total. The transcript is left intact (graduation still sees every point).
    ///
    /// Idempotent in the sense that a second immediate call returns `0`.
    pub fn claim_settlement(&mut self, agent: &AgentId) -> u64 {
        let lifetime = self.lifetime_points(agent);
        let settled = self.settled_points.entry(*agent).or_insert(0);
        let owed = lifetime.saturating_sub(*settled);
        *settled = lifetime;
        owed
    }

    /// Whether `student` has already graduated.
    pub fn is_graduated(&self, student: &AgentId) -> bool {
        self.graduated.contains(student)
    }

    /// Mark `student` graduated. Returns `Err(AlreadyGraduated)` if they were
    /// already in the set (so the caller never pays a second bonus).
    pub fn mark_graduated(&mut self, student: &AgentId) -> Result<(), RegistryError> {
        if !self.graduated.insert(*student) {
            return Err(RegistryError::AlreadyGraduated);
        }
        Ok(())
    }

    /// Count of distinct agents with any transcript.
    pub fn enrolled(&self) -> usize {
        self.ledgers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PointAward, WorkUnit};

    const PROF: AgentId = [0xA0; 32];
    const STUDENT: AgentId = [0x0E; 32];

    fn award(to: AgentId, to_role: Role, points: u64, year: u8, nonce: u64) -> SignedAward {
        // Bookkeeping tests don't exercise crypto — the sig/pubkey are unused by
        // the registry (verify happens in sigil-rpc before record_award).
        SignedAward {
            award: PointAward {
                from: PROF,
                from_role: Role::Professor,
                to,
                to_role,
                points,
                unit: WorkUnit { unit_id: format!("u{nonce}"), max_points: 100 },
                year,
                nonce,
            },
            sig: vec![],
            pubkey: vec![],
        }
    }

    #[test]
    fn record_credits_transcript() {
        let mut r = UniversityRegistry::new();
        r.mark_tuition_paid(&STUDENT, 1);
        let total = r.record_award(&award(STUDENT, Role::Student, 80, 1, 1)).unwrap();
        assert_eq!(total, 80);
        assert_eq!(r.lifetime_points(&STUDENT), 80);
    }

    #[test]
    fn replay_is_blocked() {
        let mut r = UniversityRegistry::new();
        r.mark_tuition_paid(&STUDENT, 1);
        r.record_award(&award(STUDENT, Role::Student, 80, 1, 7)).unwrap();
        // same authority + same nonce → replay rejected, transcript unchanged.
        let err = r.record_award(&award(STUDENT, Role::Student, 80, 1, 7)).unwrap_err();
        assert_eq!(err, RegistryError::ReplayedNonce { nonce: 7 });
        assert_eq!(r.lifetime_points(&STUDENT), 80, "replay must not double-credit");
    }

    #[test]
    fn student_needs_tuition_staff_do_not() {
        let mut r = UniversityRegistry::new();
        // Student without tuition for year 2 → rejected.
        let err = r.record_award(&award(STUDENT, Role::Student, 50, 2, 1)).unwrap_err();
        assert_eq!(err, RegistryError::TuitionUnpaid { year: 2 });
        // A Tutor (staff) earns with no tuition requirement.
        let tutor: AgentId = [0x77; 32];
        assert!(r.record_award(&award(tutor, Role::Tutor, 15, 2, 2)).is_ok());
    }

    #[test]
    fn settlement_high_water_mark() {
        let mut r = UniversityRegistry::new();
        r.mark_tuition_paid(&STUDENT, 1);
        r.record_award(&award(STUDENT, Role::Student, 80, 1, 1)).unwrap();
        assert_eq!(r.unsettled_points(&STUDENT), 80);
        assert_eq!(r.claim_settlement(&STUDENT), 80, "first claim pays all 80");
        assert_eq!(r.claim_settlement(&STUDENT), 0, "nothing left to settle");
        // earn more → only the new delta is settle-able.
        r.mark_tuition_paid(&STUDENT, 2);
        r.record_award(&award(STUDENT, Role::Student, 30, 2, 2)).unwrap();
        assert_eq!(r.unsettled_points(&STUDENT), 30);
        assert_eq!(r.claim_settlement(&STUDENT), 30);
        // ...but the lifetime transcript still shows all 110 for graduation.
        assert_eq!(r.lifetime_points(&STUDENT), 110);
    }

    #[test]
    fn graduate_marker_is_one_shot() {
        let mut r = UniversityRegistry::new();
        assert!(!r.is_graduated(&STUDENT));
        r.mark_graduated(&STUDENT).unwrap();
        assert!(r.is_graduated(&STUDENT));
        assert_eq!(r.mark_graduated(&STUDENT), Err(RegistryError::AlreadyGraduated));
    }

    #[test]
    fn bincode_roundtrip_is_lossless() {
        // Mirrors CreditVault's persistence test: the registry must survive a
        // flux-db round-trip byte-exactly (BTreeMaps keyed by [u8;32] — bincode,
        // not json, which would need string keys).
        let mut r = UniversityRegistry::new();
        r.mark_tuition_paid(&STUDENT, 1);
        r.record_award(&award(STUDENT, Role::Student, 80, 1, 1)).unwrap();
        r.claim_settlement(&STUDENT);
        r.mark_graduated(&[0x01; 32]).unwrap();
        let bytes = bincode::serialize(&r).unwrap();
        let back: UniversityRegistry = bincode::deserialize(&bytes).unwrap();
        assert_eq!(r, back);
    }
}
