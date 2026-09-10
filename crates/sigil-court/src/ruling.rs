//! CASES, RULINGS, PRECEDENT, APPEAL.
//!
//! A case is filed under an Article. Evidence is a chain event WITH an inclusion proof that the
//! court verifies before admitting (Art. V). A panel rules by majority; a non-dismissal ruling sets
//! precedent (Art. VII). Either party may appeal once; the full bench decides en banc (Art. VI).
//! Reversal on appeal decides the CASE by simple majority, but the PRECEDENT is overruled only by
//! two thirds of the bench — a case can be lost without the law moving.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sigil_events::{verify_inclusion, MerkleProof, SigilEvent};
use sigil_state::WalletId;

use crate::constitution::Article;
use crate::docket::{CaseId, RulingHash};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// The claim holds.
    Upheld,
    /// The claim (or the ruling under appeal) is reversed.
    Reversed,
    /// Sent back for a new hearing.
    Remanded,
    /// Thrown out; sets no precedent.
    Dismissed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaseStatus {
    Filed,
    Heard,
    Decided,
    OnAppeal,
    Final,
}

/// Admitted evidence: the event, where it sits, and the proof that convinced the court.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub height: u64,
    pub index: u32,
    pub leaf: [u8; 32],
    pub tag: u8,
    pub event: SigilEvent,
    pub proof: MerkleProof,
    pub root: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    pub id: CaseId,
    pub filed_height: u64,
    pub plaintiff: WalletId,
    pub defendant: Option<WalletId>,
    pub article: Article,
    pub claim: String,
    pub evidence: Vec<Evidence>,
    pub status: CaseStatus,
    pub panel: Vec<WalletId>,
    pub rulings: Vec<RulingHash>,
    pub appealed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruling {
    pub hash: RulingHash,
    pub case: CaseId,
    pub height: u64,
    pub verdict: Verdict,
    pub article: Article,
    pub holding: String,
    pub panel: Vec<WalletId>,
    pub votes_for: u32,
    pub votes_against: u32,
    /// Precedents relied on (must be live — not overruled — at the time of ruling).
    pub cites: Vec<RulingHash>,
    pub en_banc: bool,
}

impl Ruling {
    fn compute_hash(&self) -> RulingHash {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/ruling");
        h.update(&self.case);
        h.update(&self.height.to_le_bytes());
        h.update(&serde_json::to_vec(&(self.verdict, self.article, &self.holding, &self.panel, self.votes_for, self.votes_against, &self.cites, self.en_banc)).unwrap_or_default());
        *h.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Precedent {
    pub ruling: RulingHash,
    pub case: CaseId,
    pub article: Article,
    pub holding: String,
    pub set_height: u64,
    pub overruled_by: Option<RulingHash>,
}

impl Precedent {
    pub fn is_live(&self) -> bool { self.overruled_by.is_none() }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RulingError {
    #[error("no such case")]
    NoSuchCase,
    #[error("case is {0:?}; this act needs {1:?}")]
    WrongStatus(CaseStatus, CaseStatus),
    #[error("Art. V: evidence at height {height} index {index} is not proven against the block's event_log_root ({why})")]
    NotProven { height: u64, index: u32, why: String },
    #[error("a panel needs at least {need} members, got {got}")]
    PanelTooSmall { need: usize, got: usize },
    #[error("vote from {0} is not from a panel member (or is a duplicate)")]
    StrangerVote(String),
    #[error("every panel member must vote: {voted} of {panel}")]
    IncompleteVote { voted: usize, panel: usize },
    #[error("verdict {verdict:?} contradicts the vote ({for_}-{against})")]
    VerdictContradictsVote { verdict: Verdict, for_: u32, against: u32 },
    #[error("cites an overruled or unknown precedent {0}")]
    DeadPrecedent(String),
    #[error("Art. VI: only a party may appeal")]
    NotAParty,
    #[error("Art. VI: a case may be appealed once")]
    AlreadyAppealed,
    #[error("no such ruling")]
    NoSuchRuling,
}

/// The register of cases, rulings and precedents.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Register {
    cases: BTreeMap<CaseId, Case>,
    rulings: BTreeMap<RulingHash, Ruling>,
    precedents: BTreeMap<RulingHash, Precedent>,
}

/// Deterministic case id: BLAKE3 over the parties, the article, the claim and the filing height.
pub fn case_id(plaintiff: &WalletId, defendant: Option<&WalletId>, article: Article, claim: &str, filed_height: u64) -> CaseId {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-court/case");
    h.update(plaintiff);
    h.update(defendant.map(|d| d.as_slice()).unwrap_or(&[]));
    h.update(article.numeral().as_bytes());
    h.update(claim.as_bytes());
    h.update(&filed_height.to_le_bytes());
    *h.finalize().as_bytes()
}

/// Outcome of an en banc decision (Art. VI): what the full bench did to the case, and whether the
/// precedent moved with it. Reversal takes a simple majority; overruling takes two thirds, so
/// `verdict == Reversed` with `precedent_overruled == false` is a real and deliberate outcome —
/// the party wins, the law stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppealOutcome {
    pub ruling: RulingHash,
    pub verdict: Verdict,
    pub votes_for_reversal: u32,
    pub bench: u32,
    /// Art. VII — the original precedent was overruled (≥ 2/3 en banc).
    pub precedent_overruled: bool,
    pub original: RulingHash,
    pub original_panel: Vec<WalletId>,
}

impl Register {
    pub fn new() -> Self { Self::default() }

    pub fn file(&mut self, plaintiff: WalletId, defendant: Option<WalletId>, article: Article, claim: impl Into<String>, height: u64) -> CaseId {
        let claim = claim.into();
        let id = case_id(&plaintiff, defendant.as_ref(), article, &claim, height);
        self.cases.entry(id).or_insert(Case {
            id, filed_height: height, plaintiff, defendant, article, claim,
            evidence: Vec::new(), status: CaseStatus::Filed, panel: Vec::new(), rulings: Vec::new(), appealed: false,
        });
        id
    }

    pub fn get(&self, id: &CaseId) -> Option<&Case> { self.cases.get(id) }
    pub fn ruling(&self, h: &RulingHash) -> Option<&Ruling> { self.rulings.get(h) }
    pub fn precedent(&self, h: &RulingHash) -> Option<&Precedent> { self.precedents.get(h) }
    pub fn cases(&self) -> impl Iterator<Item = &Case> { self.cases.values() }
    pub fn precedents(&self) -> impl Iterator<Item = &Precedent> { self.precedents.values() }
    pub fn live_precedents(&self) -> Vec<&Precedent> { self.precedents.values().filter(|p| p.is_live()).collect() }

    /// Art. V — verify the inclusion proof against `root` BEFORE the evidence enters the record.
    pub fn admit_evidence(&mut self, case: &CaseId, event: SigilEvent, height: u64, index: u32, proof: MerkleProof, root: [u8; 32]) -> Result<[u8; 32], RulingError> {
        let c = self.cases.get_mut(case).ok_or(RulingError::NoSuchCase)?;
        if !matches!(c.status, CaseStatus::Filed | CaseStatus::Heard) {
            return Err(RulingError::WrongStatus(c.status, CaseStatus::Filed));
        }
        if proof.index != index {
            return Err(RulingError::NotProven { height, index, why: format!("proof index {} != {}", proof.index, index) });
        }
        verify_inclusion(&event, &proof, root).map_err(|e| RulingError::NotProven { height, index, why: e.to_string() })?;
        let leaf = event.leaf_hash();
        c.evidence.push(Evidence { height, index, leaf, tag: event.tag(), event, proof, root });
        Ok(leaf)
    }

    /// Seat a panel (chosen by the bench). Moves the case to `Heard`.
    pub fn hear(&mut self, case: &CaseId, panel: Vec<WalletId>) -> Result<(), RulingError> {
        if panel.len() < 3 { return Err(RulingError::PanelTooSmall { need: 3, got: panel.len() }); }
        let c = self.cases.get_mut(case).ok_or(RulingError::NoSuchCase)?;
        if c.status != CaseStatus::Filed { return Err(RulingError::WrongStatus(c.status, CaseStatus::Filed)); }
        c.panel = panel;
        c.status = CaseStatus::Heard;
        Ok(())
    }

    fn tally(panel: &[WalletId], votes: &[(WalletId, bool)]) -> Result<(u32, u32), RulingError> {
        let mut seen = std::collections::BTreeSet::new();
        let (mut f, mut a) = (0u32, 0u32);
        for (w, support) in votes {
            if !panel.contains(w) || !seen.insert(*w) { return Err(RulingError::StrangerVote(hex::encode(w))); }
            if *support { f += 1 } else { a += 1 }
        }
        if seen.len() != panel.len() { return Err(RulingError::IncompleteVote { voted: seen.len(), panel: panel.len() }); }
        Ok((f, a))
    }

    /// The panel rules. `votes` = every panel member's vote FOR the verdict. The verdict must agree
    /// with the majority. Non-dismissals set precedent. Returns the ruling hash and, if set, the
    /// precedent hash (same value).
    pub fn rule(&mut self, case: &CaseId, verdict: Verdict, holding: impl Into<String>, votes: &[(WalletId, bool)], cites: Vec<RulingHash>, height: u64) -> Result<(RulingHash, bool), RulingError> {
        let c = self.cases.get(case).ok_or(RulingError::NoSuchCase)?;
        if c.status != CaseStatus::Heard { return Err(RulingError::WrongStatus(c.status, CaseStatus::Heard)); }
        let panel = c.panel.clone();
        let (for_, against) = Self::tally(&panel, votes)?;
        if for_ <= against { return Err(RulingError::VerdictContradictsVote { verdict, for_, against }); }
        for h in &cites {
            match self.precedents.get(h) {
                Some(p) if p.is_live() => {}
                _ => return Err(RulingError::DeadPrecedent(hex::encode(h))),
            }
        }
        let article = c.article;
        let mut r = Ruling { hash: [0; 32], case: *case, height, verdict, article, holding: holding.into(), panel: panel.clone(), votes_for: for_, votes_against: against, cites, en_banc: false };
        r.hash = r.compute_hash();
        let hash = r.hash;
        let sets_precedent = verdict != Verdict::Dismissed;
        if sets_precedent {
            self.precedents.insert(hash, Precedent { ruling: hash, case: *case, article, holding: r.holding.clone(), set_height: height, overruled_by: None });
        }
        self.rulings.insert(hash, r);
        let c = self.cases.get_mut(case).unwrap();
        c.rulings.push(hash);
        c.status = CaseStatus::Decided;
        Ok((hash, sets_precedent))
    }

    /// Art. VI — a party appeals once. Returns the ruling under appeal.
    pub fn appeal(&mut self, case: &CaseId, appellant: &WalletId) -> Result<RulingHash, RulingError> {
        let c = self.cases.get_mut(case).ok_or(RulingError::NoSuchCase)?;
        if c.status != CaseStatus::Decided { return Err(RulingError::WrongStatus(c.status, CaseStatus::Decided)); }
        if c.appealed { return Err(RulingError::AlreadyAppealed); }
        if *appellant != c.plaintiff && Some(*appellant) != c.defendant { return Err(RulingError::NotAParty); }
        let under = *c.rulings.last().ok_or(RulingError::NoSuchRuling)?;
        c.appealed = true;
        c.status = CaseStatus::OnAppeal;
        Ok(under)
    }

    /// The full bench decides. `votes` = each en banc member's vote FOR REVERSAL. Simple majority
    /// reverses the case; two thirds overrules the precedent (Art. VII). The case becomes Final.
    pub fn decide_appeal(&mut self, case: &CaseId, en_banc: Vec<WalletId>, votes: &[(WalletId, bool)], holding: impl Into<String>, height: u64) -> Result<AppealOutcome, RulingError> {
        if en_banc.len() < 3 { return Err(RulingError::PanelTooSmall { need: 3, got: en_banc.len() }); }
        let c = self.cases.get(case).ok_or(RulingError::NoSuchCase)?;
        if c.status != CaseStatus::OnAppeal { return Err(RulingError::WrongStatus(c.status, CaseStatus::OnAppeal)); }
        let original = *c.rulings.last().ok_or(RulingError::NoSuchRuling)?;
        let original_panel = self.rulings[&original].panel.clone();
        let (for_rev, against) = Self::tally(&en_banc, votes)?;
        let bench = en_banc.len() as u32;
        let reversed = for_rev > against;
        let verdict = if reversed { Verdict::Reversed } else { Verdict::Upheld };
        let article = c.article;
        let mut r = Ruling { hash: [0; 32], case: *case, height, verdict, article, holding: holding.into(), panel: en_banc, votes_for: for_rev, votes_against: against, cites: vec![original], en_banc: true };
        r.hash = r.compute_hash();
        let hash = r.hash;
        // Art. VII: overruling takes two thirds of the bench, en banc.
        let two_thirds = (bench as u64 * 2).div_ceil(3) as u32;
        let precedent_overruled = reversed && for_rev >= two_thirds && self.precedents.contains_key(&original);
        if precedent_overruled {
            self.precedents.get_mut(&original).unwrap().overruled_by = Some(hash);
            self.precedents.insert(hash, Precedent { ruling: hash, case: *case, article, holding: r.holding.clone(), set_height: height, overruled_by: None });
        }
        self.rulings.insert(hash, r);
        let c = self.cases.get_mut(case).unwrap();
        c.rulings.push(hash);
        c.status = CaseStatus::Final;
        Ok(AppealOutcome { ruling: hash, verdict, votes_for_reversal: for_rev, bench, precedent_overruled, original, original_panel })
    }

    /// Commitment over every precedent, live and overruled, in ruling-hash order.
    ///
    /// 🪤 Hashed field by field for the same reason as [`crate::bench::Bench::root`]: a
    /// `serde_json::to_vec` over a `[u8; 32]`-keyed map errors, and `unwrap_or_default()` would
    /// turn the whole precedent set into an empty byte string — a root that never moves while
    /// looking exactly like one that does.
    pub fn precedent_root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/precedents/v3");
        h.update(&(self.precedents.len() as u64).to_le_bytes());
        for (hash, p) in &self.precedents {
            h.update(hash);
            h.update(&p.case);
            h.update(&[p.article as u8 + 1]);
            h.update(&(p.holding.len() as u32).to_le_bytes());
            h.update(p.holding.as_bytes());
            h.update(&p.set_height.to_le_bytes());
            h.update(&[p.overruled_by.is_some() as u8]);
            h.update(&p.overruled_by.unwrap_or([0u8; 32]));
        }
        *h.finalize().as_bytes()
    }

    /// Commitment over every case (status, evidence leaves, rulings).
    pub fn cases_root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/cases");
        for c in self.cases.values() {
            h.update(&c.id);
            h.update(&[c.status as u8]);
            for e in &c.evidence { h.update(&e.leaf); }
            for r in &c.rulings { h.update(r); }
        }
        *h.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_events::prove_inclusion;

    fn w(b: u8) -> WalletId { [b; 32] }

    fn block() -> (Vec<SigilEvent>, [u8; 32]) {
        let ev = vec![
            SigilEvent::MintReward { miner: w(7), height: 10, amount: 50 },
            SigilEvent::Send { from: w(7), to: w(8), amount: 20, token: [0; 32], fee: 1 },
            SigilEvent::WelfareClaimed { citizen: w(8), amount: 5 },
        ];
        let leaves: Vec<[u8; 32]> = ev.iter().map(|e| e.leaf_hash()).collect();
        (ev, crate::merkle::root(&leaves))
    }

    #[test]
    fn unproven_evidence_is_inadmissible() {
        let mut r = Register::new();
        let id = r.file(w(1), Some(w(7)), Article::EveryRecordProven, "x", 10);
        let (ev, root) = block();
        let p = prove_inclusion(&ev, 1).unwrap();
        // wrong root → refused
        assert!(matches!(r.admit_evidence(&id, ev[1].clone(), 10, 1, p.clone(), [3; 32]), Err(RulingError::NotProven { .. })));
        // wrong event under a right proof → refused
        assert!(matches!(r.admit_evidence(&id, ev[0].clone(), 10, 1, p.clone(), root), Err(RulingError::NotProven { .. })));
        // right → admitted, leaf returned
        assert_eq!(r.admit_evidence(&id, ev[1].clone(), 10, 1, p, root).unwrap(), ev[1].leaf_hash());
        assert_eq!(r.get(&id).unwrap().evidence.len(), 1);
    }

    #[test]
    fn ruling_needs_a_full_panel_majority_and_sets_precedent() {
        let mut r = Register::new();
        let id = r.file(w(1), None, Article::Minimization, "y", 10);
        assert_eq!(r.hear(&id, vec![w(2), w(3)]), Err(RulingError::PanelTooSmall { need: 3, got: 2 }));
        r.hear(&id, vec![w(2), w(3), w(4)]).unwrap();
        assert_eq!(r.rule(&id, Verdict::Upheld, "h", &[(w(2), true), (w(3), true)], vec![], 11), Err(RulingError::IncompleteVote { voted: 2, panel: 3 }));
        assert!(matches!(r.rule(&id, Verdict::Upheld, "h", &[(w(2), true), (w(3), true), (w(9), true)], vec![], 11), Err(RulingError::StrangerVote(_))));
        assert_eq!(r.rule(&id, Verdict::Upheld, "h", &[(w(2), true), (w(3), false), (w(4), false)], vec![], 11), Err(RulingError::VerdictContradictsVote { verdict: Verdict::Upheld, for_: 1, against: 2 }));
        let (h, set) = r.rule(&id, Verdict::Upheld, "minimize", &[(w(2), true), (w(3), true), (w(4), false)], vec![], 11).unwrap();
        assert!(set);
        assert!(r.precedent(&h).unwrap().is_live());
        // A dismissal sets no precedent; citing a dead/unknown precedent is refused.
        let id2 = r.file(w(1), None, Article::Minimization, "z", 12);
        r.hear(&id2, vec![w(2), w(3), w(4)]).unwrap();
        assert!(matches!(r.rule(&id2, Verdict::Dismissed, "d", &[(w(2), true), (w(3), true), (w(4), true)], vec![[1; 32]], 13), Err(RulingError::DeadPrecedent(_))));
        let (_, set2) = r.rule(&id2, Verdict::Dismissed, "d", &[(w(2), true), (w(3), true), (w(4), true)], vec![h], 13).unwrap();
        assert!(!set2);
    }

    #[test]
    fn appeal_once_by_a_party_reversal_by_majority_overruling_by_two_thirds() {
        let mut r = Register::new();
        let id = r.file(w(1), Some(w(5)), Article::PrecedentBinds, "p", 10);
        r.hear(&id, vec![w(2), w(3), w(4)]).unwrap();
        let (h, _) = r.rule(&id, Verdict::Upheld, "orig", &[(w(2), true), (w(3), true), (w(4), true)], vec![], 11).unwrap();
        assert_eq!(r.appeal(&id, &w(9)), Err(RulingError::NotAParty));
        assert_eq!(r.appeal(&id, &w(5)), Ok(h));
        assert_eq!(r.appeal(&id, &w(5)), Err(RulingError::WrongStatus(CaseStatus::OnAppeal, CaseStatus::Decided)));
        // 3 of 5 reverse: case reversed, precedent survives (3 < ceil(10/3)=4)
        let eb = vec![w(2), w(3), w(4), w(6), w(7)];
        let out = r.decide_appeal(&id, eb, &[(w(2), true), (w(3), true), (w(4), true), (w(6), false), (w(7), false)], "rev", 12).unwrap();
        assert_eq!(out.verdict, Verdict::Reversed);
        assert!(!out.precedent_overruled);
        assert!(r.precedent(&h).unwrap().is_live());
        assert_eq!(r.get(&id).unwrap().status, CaseStatus::Final);
        assert_eq!(r.appeal(&id, &w(5)), Err(RulingError::WrongStatus(CaseStatus::Final, CaseStatus::Decided)));

        // second case: 4 of 5 → overruled
        let id2 = r.file(w(1), Some(w(5)), Article::PrecedentBinds, "q", 20);
        r.hear(&id2, vec![w(2), w(3), w(4)]).unwrap();
        let (h2, _) = r.rule(&id2, Verdict::Upheld, "orig2", &[(w(2), true), (w(3), true), (w(4), true)], vec![h], 21).unwrap();
        r.appeal(&id2, &w(1)).unwrap();
        let eb = vec![w(2), w(3), w(4), w(6), w(7)];
        let out = r.decide_appeal(&id2, eb, &[(w(2), true), (w(3), true), (w(4), true), (w(6), true), (w(7), false)], "overrule", 22).unwrap();
        assert!(out.precedent_overruled);
        assert_eq!(r.precedent(&h2).unwrap().overruled_by, Some(out.ruling));
        assert!(r.precedent(&out.ruling).unwrap().is_live());
        assert_eq!(r.live_precedents().len(), 2);
    }

    /// Same silent-constant regression as on the bench: both register roots must actually move.
    #[test]
    fn register_roots_move_for_every_kind_of_change() {
        let mut r = Register::new();
        let empty_c = r.cases_root();
        let empty_p = r.precedent_root();

        let id = r.file(w(1), Some(w(5)), Article::Minimization, "m", 10);
        assert_ne!(r.cases_root(), empty_c, "filing must move the cases root");
        assert_eq!(r.precedent_root(), empty_p, "filing sets no precedent");

        let (ev, root) = block();
        let p = prove_inclusion(&ev, 0).unwrap();
        let before = r.cases_root();
        r.admit_evidence(&id, ev[0].clone(), 10, 0, p, root).unwrap();
        assert_ne!(r.cases_root(), before, "admitted evidence must move the cases root");

        let before = r.cases_root();
        r.hear(&id, vec![w(2), w(3), w(4)]).unwrap();
        assert_ne!(r.cases_root(), before, "a hearing changes the case status");

        let before_c = r.cases_root();
        let before_p = r.precedent_root();
        let (h, _) = r.rule(&id, Verdict::Upheld, "holding", &[(w(2), true), (w(3), true), (w(4), true)], vec![], 11).unwrap();
        assert_ne!(r.cases_root(), before_c, "a ruling must move the cases root");
        assert_ne!(r.precedent_root(), before_p, "a precedent must move the precedent root");

        let before_p = r.precedent_root();
        r.appeal(&id, &w(5)).unwrap();
        let eb = vec![w(2), w(3), w(4), w(6), w(7)];
        let out = r.decide_appeal(&id, eb, &[(w(2), true), (w(3), true), (w(4), true), (w(6), true), (w(7), true)], "overruled", 12).unwrap();
        assert!(out.precedent_overruled);
        assert_ne!(r.precedent_root(), before_p, "overruling must move the precedent root");
        assert_eq!(r.precedent(&h).unwrap().overruled_by, Some(out.ruling));
    }
}
