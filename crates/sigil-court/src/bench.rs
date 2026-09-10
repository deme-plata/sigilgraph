//! THE BENCH — the highest place in the nation to be educated and promoted.
//!
//! Art. VIII: promotion by deeds and examination, never solo. A candidate climbs
//! `Clerk → Advocate → Magistrate → Justice → ChiefJustice`. Each step needs (a) credentials
//! earned by sitting exams (or graduating from SIGIL University), (b) a deeds record — panels sat
//! and rulings that survived appeal — and (c) a quorum of the sitting bench voting for it. The
//! rules are pure functions on this type; the court (lib.rs) records the outcome on the docket.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sigil_state::WalletId;
use sigil_university::{AgentId, UniversityRegistry};

use crate::docket::CaseId;

/// The ladder. Ordering is the ordering of authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Rank {
    Clerk,
    Advocate,
    Magistrate,
    Justice,
    ChiefJustice,
}

impl Rank {
    pub fn next(self) -> Option<Rank> {
        match self {
            Rank::Clerk => Some(Rank::Advocate),
            Rank::Advocate => Some(Rank::Magistrate),
            Rank::Magistrate => Some(Rank::Justice),
            Rank::Justice => Some(Rank::ChiefJustice),
            Rank::ChiefJustice => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Rank::Clerk => "Clerk",
            Rank::Advocate => "Advocate",
            Rank::Magistrate => "Magistrate",
            Rank::Justice => "Justice",
            Rank::ChiefJustice => "Chief Justice",
        }
    }
    /// May sit on a ruling panel (Magistrate and above).
    pub fn may_sit(self) -> bool { self >= Rank::Magistrate }
}

/// The curriculum. Passing all five is the full education of a Justice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Course {
    ConstitutionalCode,
    Evidence,
    DisclosureLaw,
    PrivacyDoctrine,
    MonetaryLaw,
}

impl Course {
    pub const ALL: [Course; 5] = [
        Course::ConstitutionalCode,
        Course::Evidence,
        Course::DisclosureLaw,
        Course::PrivacyDoctrine,
        Course::MonetaryLaw,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Course::ConstitutionalCode => "Constitutional Code",
            Course::Evidence => "Evidence & Inclusion Proofs",
            Course::DisclosureLaw => "Disclosure Law",
            Course::PrivacyDoctrine => "Privacy Doctrine",
            Course::MonetaryLaw => "Monetary Law",
        }
    }
}

/// Pass mark for any exam, in basis points of the maximum score.
pub const PASS_BPS: u16 = 7_000;

/// What a justice holds. `Course(_)` from exams, `UniversityGraduate` from the registrar,
/// `BarAdmitted` is derived (Constitutional Code + Evidence both passed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Credential {
    Course(Course),
    UniversityGraduate,
    BarAdmitted,
    /// **Æresborger** — honorary citizen of the SIGIL Nation, the court's recognition that this
    /// wallet bears an Order of the nation (`sigil_events::SigilOrder`). Soulbound: earned by
    /// deeds, never bought, never transferred.
    ///
    /// It is NOT a rank and confers no power to rule — an æresborger who is not a Magistrate still
    /// cannot sit on a panel. Honour and authority are deliberately different axes here; conflating
    /// them is how an honours system becomes a peerage.
    Aeresborger,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Justice {
    pub wallet: WalletId,
    pub rank: Rank,
    pub appointed_height: u64,
    pub credentials: BTreeSet<Credential>,
    /// Panels this justice has sat on (deeds).
    pub panels_sat: u32,
    /// Rulings this justice joined that survived (or were never) appealed.
    pub upheld: u32,
    /// Rulings this justice joined that were reversed en banc.
    pub reversed: u32,
    pub recused: BTreeSet<CaseId>,
}

impl Justice {
    pub fn has(&self, c: Credential) -> bool { self.credentials.contains(&c) }
    /// upheld / (upheld + reversed), in bps; 10_000 when nothing has been decided against them.
    pub fn upheld_bps(&self) -> u32 {
        let decided = self.upheld + self.reversed;
        if decided == 0 { 10_000 } else { (self.upheld as u64 * 10_000 / decided as u64) as u32 }
    }
}

/// What it takes to reach a rank. Pure data so the requirements are readable on the docket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirements {
    pub credentials: Vec<Credential>,
    pub min_panels_sat: u32,
    pub min_upheld_bps: u32,
    /// Only members at or above this rank may vote on the promotion.
    pub electorate_min_rank: Rank,
    /// Fraction of the electorate that must vote FOR: numerator/denominator.
    pub quorum: (u32, u32),
}

impl Requirements {
    pub fn for_rank(to: Rank) -> Option<Requirements> {
        Some(match to {
            Rank::Clerk => return None,
            Rank::Advocate => Requirements {
                credentials: vec![Credential::BarAdmitted],
                min_panels_sat: 0,
                min_upheld_bps: 0,
                electorate_min_rank: Rank::Magistrate,
                quorum: (1, 3), // one third of Magistrate+, never zero (Art. VIII)
            },
            Rank::Magistrate => Requirements {
                credentials: vec![Credential::BarAdmitted, Credential::Course(Course::DisclosureLaw), Credential::Course(Course::PrivacyDoctrine)],
                min_panels_sat: 0,
                min_upheld_bps: 0,
                electorate_min_rank: Rank::Justice,
                quorum: (1, 2),
            },
            Rank::Justice => Requirements {
                credentials: Course::ALL.iter().map(|c| Credential::Course(*c)).collect(),
                min_panels_sat: 3,
                min_upheld_bps: 6_667,
                electorate_min_rank: Rank::Justice,
                quorum: (2, 3),
            },
            Rank::ChiefJustice => Requirements {
                credentials: vec![],
                min_panels_sat: 5,
                min_upheld_bps: 6_667,
                electorate_min_rank: Rank::Justice,
                quorum: (2, 3),
            },
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BenchError {
    #[error("not a member of the bench")]
    NotMember,
    #[error("already a member of the bench")]
    AlreadyMember,
    #[error("cannot promote past Chief Justice")]
    TopOfLadder,
    #[error("promotion must be to the next rank ({expected:?}), not {got:?}")]
    SkipsRank { expected: Rank, got: Rank },
    #[error("missing credential {0:?}")]
    MissingCredential(Credential),
    #[error("deeds insufficient: panels_sat {panels_sat} < {need}")]
    TooFewPanels { panels_sat: u32, need: u32 },
    #[error("upheld ratio {have_bps} bps < {need_bps} bps")]
    UpheldTooLow { have_bps: u32, need_bps: u32 },
    #[error("voter {0} is not eligible (not a member, below the electorate rank, the candidate, or a duplicate)")]
    IneligibleVoter(String),
    #[error("quorum not met: {votes} of {electorate} eligible, need {need}")]
    QuorumNotMet { votes: u32, electorate: u32, need: u32 },
    #[error("Art. VIII: a rank is never granted solo — zero votes")]
    NoVotes,
    #[error("exam score {0} bps is out of range")]
    BadScore(u16),
    #[error("{0:?} has not graduated from SIGIL University")]
    NotGraduated(String),
    #[error("already holds {0:?}")]
    AlreadyHolds(Credential),
    #[error("not enough eligible members for a panel of {want}: have {have}")]
    PanelTooSmall { want: usize, have: usize },
}

/// The sitting bench.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bench {
    members: BTreeMap<WalletId, Justice>,
    chief: Option<WalletId>,
}

impl Bench {
    pub fn new() -> Self { Self::default() }

    /// Seat a member at `rank` (appointment by the founding act; later ranks come from promotion).
    pub fn appoint(&mut self, wallet: WalletId, rank: Rank, height: u64) -> Result<(), BenchError> {
        if self.members.contains_key(&wallet) { return Err(BenchError::AlreadyMember); }
        let mut j = Justice { wallet, rank, appointed_height: height, credentials: BTreeSet::new(), panels_sat: 0, upheld: 0, reversed: 0, recused: BTreeSet::new() };
        if rank == Rank::ChiefJustice {
            if let Some(old) = self.chief.take() {
                if let Some(o) = self.members.get_mut(&old) { o.rank = Rank::Justice; }
            }
            self.chief = Some(wallet);
        }
        // A founding appointment at Magistrate+ implies the education the rank demands.
        if rank >= Rank::Magistrate {
            for c in Course::ALL { j.credentials.insert(Credential::Course(c)); }
            j.credentials.insert(Credential::BarAdmitted);
        }
        self.members.insert(wallet, j);
        Ok(())
    }

    pub fn get(&self, w: &WalletId) -> Option<&Justice> { self.members.get(w) }
    pub fn is_member(&self, w: &WalletId) -> bool { self.members.contains_key(w) }
    pub fn chief(&self) -> Option<WalletId> { self.chief }
    pub fn len(&self) -> usize { self.members.len() }
    pub fn is_empty(&self) -> bool { self.members.is_empty() }
    pub fn members(&self) -> impl Iterator<Item = &Justice> { self.members.values() }
    pub fn rank_of(&self, w: &WalletId) -> Option<Rank> { self.members.get(w).map(|j| j.rank) }

    /// Members at or above `min` (sorted by wallet — deterministic).
    pub fn at_least(&self, min: Rank) -> Vec<WalletId> {
        self.members.values().filter(|j| j.rank >= min).map(|j| j.wallet).collect()
    }

    /// Sit an exam. Returns `passed`. A pass confers `Course(course)`; passing both
    /// Constitutional Code and Evidence confers `BarAdmitted`. Retakes are allowed (Art. VIII says
    /// nothing against trying again — only against being handed it).
    pub fn sit_exam(&mut self, w: &WalletId, course: Course, score_bps: u16) -> Result<(bool, Vec<Credential>), BenchError> {
        if score_bps > 10_000 { return Err(BenchError::BadScore(score_bps)); }
        let j = self.members.get_mut(w).ok_or(BenchError::NotMember)?;
        let passed = score_bps >= PASS_BPS;
        let mut conferred = Vec::new();
        if passed && j.credentials.insert(Credential::Course(course)) {
            conferred.push(Credential::Course(course));
            if j.has(Credential::Course(Course::ConstitutionalCode)) && j.has(Credential::Course(Course::Evidence)) && j.credentials.insert(Credential::BarAdmitted) {
                conferred.push(Credential::BarAdmitted);
            }
        }
        Ok((passed, conferred))
    }

    /// The registrar's word: a SIGIL University graduate holds `UniversityGraduate`.
    pub fn admit_graduate(&mut self, w: &WalletId, registry: &UniversityRegistry) -> Result<Credential, BenchError> {
        let agent: AgentId = *w;
        if !registry.is_graduated(&agent) { return Err(BenchError::NotGraduated(hex::encode(w))); }
        let j = self.members.get_mut(w).ok_or(BenchError::NotMember)?;
        if !j.credentials.insert(Credential::UniversityGraduate) { return Err(BenchError::AlreadyHolds(Credential::UniversityGraduate)); }
        Ok(Credential::UniversityGraduate)
    }

    /// Check every requirement for promoting `candidate` to `to` given `voters`; on success apply
    /// it and return `(from, to, votes_for, electorate)`.
    pub fn promote(&mut self, candidate: &WalletId, to: Rank, voters: &[WalletId]) -> Result<(Rank, Rank, u32, u32), BenchError> {
        let from = self.rank_of(candidate).ok_or(BenchError::NotMember)?;
        let expected = from.next().ok_or(BenchError::TopOfLadder)?;
        if expected != to { return Err(BenchError::SkipsRank { expected, got: to }); }
        let req = Requirements::for_rank(to).ok_or(BenchError::TopOfLadder)?;
        let j = &self.members[candidate];
        for c in &req.credentials {
            if !j.has(*c) { return Err(BenchError::MissingCredential(*c)); }
        }
        if j.panels_sat < req.min_panels_sat { return Err(BenchError::TooFewPanels { panels_sat: j.panels_sat, need: req.min_panels_sat }); }
        if j.upheld_bps() < req.min_upheld_bps { return Err(BenchError::UpheldTooLow { have_bps: j.upheld_bps(), need_bps: req.min_upheld_bps }); }
        if voters.is_empty() { return Err(BenchError::NoVotes); }
        let electorate: Vec<WalletId> = self.at_least(req.electorate_min_rank).into_iter().filter(|w| w != candidate).collect();
        let mut seen = BTreeSet::new();
        for v in voters {
            if v == candidate || !electorate.contains(v) || !seen.insert(*v) {
                return Err(BenchError::IneligibleVoter(hex::encode(v)));
            }
        }
        let votes = seen.len() as u32;
        let n = electorate.len() as u32;
        // need = ceil(n * num / den), and never zero (Art. VIII)
        let need = ((n as u64 * req.quorum.0 as u64).div_ceil(req.quorum.1 as u64) as u32).max(1);
        if votes < need { return Err(BenchError::QuorumNotMet { votes, electorate: n, need }); }
        if to == Rank::ChiefJustice {
            if let Some(old) = self.chief.take() {
                if let Some(o) = self.members.get_mut(&old) { o.rank = Rank::Justice; }
            }
            self.chief = Some(*candidate);
        }
        self.members.get_mut(candidate).unwrap().rank = to;
        Ok((from, to, votes, n))
    }

    /// Record that `w` bears an Order of the nation. Called ONLY after
    /// [`sigil_events::HonorPolicy`] has already accepted the conferral — this method is the
    /// bookkeeping, not the gate.
    pub fn record_honour(&mut self, w: &WalletId) -> Result<Credential, BenchError> {
        let j = self.members.get_mut(w).ok_or(BenchError::NotMember)?;
        if !j.credentials.insert(Credential::Aeresborger) {
            return Err(BenchError::AlreadyHolds(Credential::Aeresborger));
        }
        Ok(Credential::Aeresborger)
    }

    pub fn recuse(&mut self, w: &WalletId, case: CaseId) -> Result<(), BenchError> {
        let j = self.members.get_mut(w).ok_or(BenchError::NotMember)?;
        j.recused.insert(case);
        Ok(())
    }

    /// Deterministic panel selection for `case`: eligible = `may_sit()` and not recused; ordered by
    /// BLAKE3(case ‖ wallet) so no one can pick their own judge and every node picks the same panel.
    pub fn select_panel(&self, case: &CaseId, size: usize) -> Result<Vec<WalletId>, BenchError> {
        let mut eligible: Vec<(WalletId, [u8; 32])> = self
            .members
            .values()
            .filter(|j| j.rank.may_sit() && !j.recused.contains(case))
            .map(|j| {
                let mut h = blake3::Hasher::new();
                h.update(b"sigil-court/panel");
                h.update(case);
                h.update(&j.wallet);
                (j.wallet, *h.finalize().as_bytes())
            })
            .collect();
        if eligible.len() < size { return Err(BenchError::PanelTooSmall { want: size, have: eligible.len() }); }
        eligible.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(eligible.into_iter().take(size).map(|(w, _)| w).collect())
    }

    /// The full bench that sits en banc: every Justice and the Chief, not recused.
    pub fn en_banc(&self, case: &CaseId) -> Vec<WalletId> {
        self.members.values().filter(|j| j.rank >= Rank::Justice && !j.recused.contains(case)).map(|j| j.wallet).collect()
    }

    /// Presiding member of a panel: the highest rank, ties by wallet order.
    pub fn presiding(&self, panel: &[WalletId]) -> Option<WalletId> {
        panel.iter().max_by_key(|w| (self.rank_of(w), std::cmp::Reverse(**w))).copied()
    }

    pub(crate) fn record_panel(&mut self, panel: &[WalletId]) {
        for w in panel { if let Some(j) = self.members.get_mut(w) { j.panels_sat += 1; } }
    }
    pub(crate) fn record_upheld(&mut self, panel: &[WalletId]) {
        for w in panel { if let Some(j) = self.members.get_mut(w) { j.upheld += 1; } }
    }
    pub(crate) fn record_reversed(&mut self, panel: &[WalletId]) {
        for w in panel { if let Some(j) = self.members.get_mut(w) { j.reversed += 1; } }
    }

    /// Commitment over the whole bench: every member's rank, credentials, deeds and recusals, in
    /// wallet order, plus the Chief.
    ///
    /// 🪤 This is hashed FIELD BY FIELD on purpose. The obvious implementation —
    /// `serde_json::to_vec(&self.members)` — is a silent constant: `members` is keyed by
    /// `[u8; 32]`, serde_json refuses non-string map keys, and `unwrap_or_default()` turns that
    /// error into an empty `Vec`. The root then never moves, and `court_root` stops committing
    /// promotions and exams while still looking like it works.
    /// [`tests::root_moves_for_every_kind_of_change`] is the regression that caught it.
    pub fn root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/bench/v3");
        h.update(&(self.members.len() as u64).to_le_bytes());
        for (w, j) in &self.members {
            h.update(w);
            h.update(&[j.rank as u8]);
            h.update(&j.appointed_height.to_le_bytes());
            h.update(&(j.credentials.len() as u32).to_le_bytes());
            for c in &j.credentials {
                h.update(&credential_tag(*c));
            }
            h.update(&j.panels_sat.to_le_bytes());
            h.update(&j.upheld.to_le_bytes());
            h.update(&j.reversed.to_le_bytes());
            h.update(&(j.recused.len() as u32).to_le_bytes());
            for c in &j.recused {
                h.update(c);
            }
        }
        h.update(&[self.chief.is_some() as u8]);
        h.update(&self.chief.unwrap_or([0u8; 32]));
        *h.finalize().as_bytes()
    }
}

/// Two stable bytes per credential — what the bench root commits.
fn credential_tag(c: Credential) -> [u8; 2] {
    match c {
        Credential::Course(course) => [1, course as u8],
        Credential::UniversityGraduate => [2, 0],
        Credential::BarAdmitted => [3, 0],
        Credential::Aeresborger => [4, 0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(b: u8) -> WalletId { [b; 32] }

    fn seeded() -> Bench {
        let mut b = Bench::new();
        b.appoint(w(1), Rank::ChiefJustice, 1).unwrap();
        b.appoint(w(2), Rank::Justice, 1).unwrap();
        b.appoint(w(3), Rank::Justice, 1).unwrap();
        b.appoint(w(4), Rank::Magistrate, 1).unwrap();
        b.appoint(w(9), Rank::Clerk, 1).unwrap();
        b
    }

    #[test]
    fn exams_confer_courses_and_the_bar() {
        let mut b = seeded();
        let (p, c) = b.sit_exam(&w(9), Course::ConstitutionalCode, 6_999).unwrap();
        assert!(!p && c.is_empty());
        let (p, c) = b.sit_exam(&w(9), Course::ConstitutionalCode, 7_000).unwrap();
        assert!(p);
        assert_eq!(c, vec![Credential::Course(Course::ConstitutionalCode)]);
        let (_, c) = b.sit_exam(&w(9), Course::Evidence, 9_000).unwrap();
        assert_eq!(c, vec![Credential::Course(Course::Evidence), Credential::BarAdmitted]);
        assert!(matches!(b.sit_exam(&w(9), Course::Evidence, 10_001), Err(BenchError::BadScore(_))));
    }

    #[test]
    fn promotion_needs_credentials_then_quorum_never_solo() {
        let mut b = seeded();
        assert_eq!(b.promote(&w(9), Rank::Advocate, &[w(1)]), Err(BenchError::MissingCredential(Credential::BarAdmitted)));
        b.sit_exam(&w(9), Course::ConstitutionalCode, 8_000).unwrap();
        b.sit_exam(&w(9), Course::Evidence, 8_000).unwrap();
        assert_eq!(b.promote(&w(9), Rank::Advocate, &[]), Err(BenchError::NoVotes));
        assert!(matches!(b.promote(&w(9), Rank::Advocate, &[w(9)]), Err(BenchError::IneligibleVoter(_))), "cannot vote for yourself");
        assert!(matches!(b.promote(&w(9), Rank::Advocate, &[w(1), w(1)]), Err(BenchError::IneligibleVoter(_))), "no double votes");
        assert_eq!(b.promote(&w(9), Rank::Magistrate, &[w(1)]), Err(BenchError::SkipsRank { expected: Rank::Advocate, got: Rank::Magistrate }));
        // electorate for Advocate = Magistrate+ = 4 members → need ceil(4/3) = 2
        assert_eq!(b.promote(&w(9), Rank::Advocate, &[w(1)]), Err(BenchError::QuorumNotMet { votes: 1, electorate: 4, need: 2 }));
        assert_eq!(b.promote(&w(9), Rank::Advocate, &[w(1), w(4)]), Ok((Rank::Clerk, Rank::Advocate, 2, 4)));
        assert_eq!(b.rank_of(&w(9)), Some(Rank::Advocate));
    }

    #[test]
    fn chief_justice_is_singular_and_the_old_chief_steps_down() {
        let mut b = seeded();
        // w(2) → Chief needs panels ≥5 and 2/3 of the other Justice+ (w1,w3 → need 2)
        for _ in 0..5 { b.record_panel(&[w(2)]); }
        assert_eq!(b.promote(&w(2), Rank::ChiefJustice, &[w(1)]), Err(BenchError::QuorumNotMet { votes: 1, electorate: 2, need: 2 }));
        b.promote(&w(2), Rank::ChiefJustice, &[w(1), w(3)]).unwrap();
        assert_eq!(b.chief(), Some(w(2)));
        assert_eq!(b.rank_of(&w(1)), Some(Rank::Justice));
        assert_eq!(b.promote(&w(2), Rank::ChiefJustice, &[w(1)]), Err(BenchError::TopOfLadder));
    }

    #[test]
    fn panel_selection_is_deterministic_and_honours_recusal() {
        let mut b = seeded();
        let case = [5u8; 32];
        let p1 = b.select_panel(&case, 3).unwrap();
        let p2 = b.select_panel(&case, 3).unwrap();
        assert_eq!(p1, p2);
        assert!(p1.iter().all(|w| b.rank_of(w).unwrap().may_sit()));
        for m in &p1 { b.recuse(m, case).unwrap(); }
        let p3 = b.select_panel(&case, 1).unwrap();
        assert!(!p1.contains(&p3[0]));
        assert_eq!(b.select_panel(&case, 3), Err(BenchError::PanelTooSmall { want: 3, have: 1 }));
        assert_eq!(b.en_banc(&[6u8; 32]).len(), 3);
    }

    #[test]
    fn graduate_credential_needs_the_registrar() {
        let mut b = seeded();
        let mut reg = UniversityRegistry::new();
        assert!(matches!(b.admit_graduate(&w(9), &reg), Err(BenchError::NotGraduated(_))));
        reg.mark_graduated(&w(9)).unwrap();
        assert_eq!(b.admit_graduate(&w(9), &reg), Ok(Credential::UniversityGraduate));
        assert_eq!(b.admit_graduate(&w(9), &reg), Err(BenchError::AlreadyHolds(Credential::UniversityGraduate)));
    }

    /// The regression for the silent-constant root. Each of these is a real change to the bench,
    /// and each MUST move the root — a root that only moves for some of them is worse than none,
    /// because `court_root` would then attest a bench that had quietly changed underneath it.
    /// Honour and authority are separate axes. An æresborger who is a Clerk still cannot sit, and
    /// the honour does not substitute for any credential a promotion demands.
    #[test]
    fn an_honour_is_not_a_rank_and_opens_no_door() {
        let mut b = seeded(); // seeded() already sits w(9) as a Clerk
        assert_eq!(b.rank_of(&w(9)), Some(Rank::Clerk));
        assert_eq!(b.record_honour(&w(9)), Ok(Credential::Aeresborger));
        assert_eq!(b.record_honour(&w(9)), Err(BenchError::AlreadyHolds(Credential::Aeresborger)));
        assert!(b.get(&w(9)).unwrap().has(Credential::Aeresborger));
        assert!(!b.rank_of(&w(9)).unwrap().may_sit(), "an honour does not seat anyone");
        // Still refused for lack of the bar — the honour buys no shortcut up the ladder.
        assert_eq!(
            b.promote(&w(9), Rank::Advocate, &[w(1), w(4)]),
            Err(BenchError::MissingCredential(Credential::BarAdmitted))
        );
        assert_eq!(b.record_honour(&w(77)), Err(BenchError::NotMember));
    }

    #[test]
    fn root_moves_for_every_kind_of_change() {
        let empty = Bench::new().root();
        let mut b = seeded();
        let seated = b.root();
        assert_ne!(seated, empty, "a seated bench must not hash like an empty one");

        let r = b.root();
        b.sit_exam(&w(9), Course::MonetaryLaw, 9_000).unwrap();
        assert_ne!(r, b.root(), "a passed exam must move the root");

        let r = b.root();
        b.sit_exam(&w(9), Course::MonetaryLaw, 100).unwrap();
        assert_eq!(r, b.root(), "a failed retake changes nothing on the bench itself");

        let r = b.root();
        b.record_panel(&[w(2)]);
        assert_ne!(r, b.root(), "deeds must move the root");

        let r = b.root();
        b.record_upheld(&[w(2)]);
        assert_ne!(r, b.root(), "an upheld ruling must move the root");

        let r = b.root();
        b.record_reversed(&[w(2)]);
        assert_ne!(r, b.root(), "a reversal must move the root");

        let r = b.root();
        b.recuse(&w(3), [1; 32]).unwrap();
        assert_ne!(r, b.root(), "a recusal must move the root");

        let r = b.root();
        b.appoint(w(50), Rank::Clerk, 9).unwrap();
        assert_ne!(r, b.root(), "an appointment must move the root");

        // A promotion moves it, and two independently-built identical benches agree.
        let mut b2 = seeded();
        b2.sit_exam(&w(9), Course::ConstitutionalCode, 9_000).unwrap();
        b2.sit_exam(&w(9), Course::Evidence, 9_000).unwrap();
        let before = b2.root();
        b2.promote(&w(9), Rank::Advocate, &[w(1), w(4)]).unwrap();
        assert_ne!(before, b2.root(), "a promotion must move the root");

        let mut b3 = seeded();
        b3.sit_exam(&w(9), Course::ConstitutionalCode, 9_000).unwrap();
        b3.sit_exam(&w(9), Course::Evidence, 9_000).unwrap();
        b3.promote(&w(9), Rank::Advocate, &[w(1), w(4)]).unwrap();
        assert_eq!(b2.root(), b3.root(), "the same history must replay to the same root");
    }
}
