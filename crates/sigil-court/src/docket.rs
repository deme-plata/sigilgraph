//! THE DOCKET — the court's event database.
//!
//! Every act of the court is a [`CourtEvent`], appended to a hash-chained, Merkle-rooted,
//! append-only [`Docket`]. Two commitments come out of it: the chain **head** (each entry hashes
//! its predecessor, so the order is fixed) and the **docket root** (a balanced Merkle root, so any
//! single entry has an O(log n) inclusion proof). Art. IX: supersede, never delete — there is no
//! remove API on this type, by construction.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sigil_events::MerkleProof;
use sigil_state::WalletId;

use crate::bench::{Course, Credential, Rank};
use crate::constitution::Article;
use crate::disclosure::Purpose;
use crate::merkle;
use crate::ruling::Verdict;

/// 32-byte case id (BLAKE3-derived, see `ruling::case_id`).
pub type CaseId = [u8; 32];
/// 32-byte ruling hash.
pub type RulingHash = [u8; 32];
/// 32-byte disclosure-order id.
pub type OrderId = [u8; 32];

/// Every act the court can record. Canonical JSON, tagged by `kind`; append new variants at the
/// END — `tag()` numbering is stable for indexers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CourtEvent {
    CaseFiled { case: CaseId, plaintiff: WalletId, defendant: Option<WalletId>, article: Article, claim: String },
    /// A chain event admitted as evidence; `proven` is always true when recorded (Art. V — the
    /// court refuses to record unproven evidence, this field exists so the record says so).
    EvidenceAdmitted { case: CaseId, height: u64, index: u32, leaf: [u8; 32], tag: u8, proven: bool },
    HearingHeld { case: CaseId, panel: Vec<WalletId>, presiding: WalletId },
    RulingIssued { case: CaseId, ruling: RulingHash, verdict: Verdict, article: Article, votes_for: u32, votes_against: u32, cites: Vec<RulingHash> },
    AppealFiled { case: CaseId, appellant: WalletId, of_ruling: RulingHash },
    AppealDecided { case: CaseId, ruling: RulingHash, verdict: Verdict, votes_for: u32, bench: u32 },
    PrecedentSet { ruling: RulingHash, article: Article, holding: String },
    PrecedentOverruled { ruling: RulingHash, by: RulingHash, votes_for: u32, bench: u32 },
    JusticeAppointed { justice: WalletId, rank: Rank },
    Promoted { justice: WalletId, from: Rank, to: Rank, votes_for: u32, electorate: u32 },
    Recused { justice: WalletId, case: CaseId, reason: String },
    ExamSat { candidate: WalletId, course: Course, score_bps: u16, passed: bool },
    CredentialConferred { holder: WalletId, credential: Credential, source: String },
    DisclosureOrdered { order: OrderId, jurisdiction: String, purpose: Purpose, subjects: u32, from_height: u64, to_height: u64, expires_ts: u64, votes_for: u32, panel: u32 },
    DisclosureExported { order: OrderId, packet_root: [u8; 32], records: u32, redacted_fields: u32, viewing_grants: u32 },
    DisclosureRefused { order: OrderId, article: Article, reason: String },
    DisclosureRevoked { order: OrderId, reason: String },
    ContemptRecorded { wallet: WalletId, case: CaseId, reason: String },
    /// Art. IX — a prior entry is wrong; this one supersedes it. The old entry stays.
    Superseded { seq: u64, reason: String },
    /// An Order of the nation conferred — **æresborger**. Soulbound, earned by the cited deeds,
    /// and gated by `sigil_events::HonorPolicy`: the Elephant needs the operator AND a quorum, so
    /// not even the operator can confer it alone.
    HonourConferred { order: String, rank: String, recipient: WalletId, citation: String, conferred_by: WalletId, approvals: u32, operator_cosigned: bool },
}

impl CourtEvent {
    /// Stable type tag (append-only numbering).
    pub fn tag(&self) -> u8 {
        match self {
            CourtEvent::CaseFiled { .. } => 0,
            CourtEvent::EvidenceAdmitted { .. } => 1,
            CourtEvent::HearingHeld { .. } => 2,
            CourtEvent::RulingIssued { .. } => 3,
            CourtEvent::AppealFiled { .. } => 4,
            CourtEvent::AppealDecided { .. } => 5,
            CourtEvent::PrecedentSet { .. } => 6,
            CourtEvent::PrecedentOverruled { .. } => 7,
            CourtEvent::JusticeAppointed { .. } => 8,
            CourtEvent::Promoted { .. } => 9,
            CourtEvent::Recused { .. } => 10,
            CourtEvent::ExamSat { .. } => 11,
            CourtEvent::CredentialConferred { .. } => 12,
            CourtEvent::DisclosureOrdered { .. } => 13,
            CourtEvent::DisclosureExported { .. } => 14,
            CourtEvent::DisclosureRefused { .. } => 15,
            CourtEvent::DisclosureRevoked { .. } => 16,
            CourtEvent::ContemptRecorded { .. } => 17,
            CourtEvent::Superseded { .. } => 18,
            CourtEvent::HonourConferred { .. } => 19,
        }
    }

    /// Human name of the variant.
    pub fn name(&self) -> &'static str {
        match self {
            CourtEvent::CaseFiled { .. } => "CaseFiled",
            CourtEvent::EvidenceAdmitted { .. } => "EvidenceAdmitted",
            CourtEvent::HearingHeld { .. } => "HearingHeld",
            CourtEvent::RulingIssued { .. } => "RulingIssued",
            CourtEvent::AppealFiled { .. } => "AppealFiled",
            CourtEvent::AppealDecided { .. } => "AppealDecided",
            CourtEvent::PrecedentSet { .. } => "PrecedentSet",
            CourtEvent::PrecedentOverruled { .. } => "PrecedentOverruled",
            CourtEvent::JusticeAppointed { .. } => "JusticeAppointed",
            CourtEvent::Promoted { .. } => "Promoted",
            CourtEvent::Recused { .. } => "Recused",
            CourtEvent::ExamSat { .. } => "ExamSat",
            CourtEvent::CredentialConferred { .. } => "CredentialConferred",
            CourtEvent::DisclosureOrdered { .. } => "DisclosureOrdered",
            CourtEvent::DisclosureExported { .. } => "DisclosureExported",
            CourtEvent::DisclosureRefused { .. } => "DisclosureRefused",
            CourtEvent::DisclosureRevoked { .. } => "DisclosureRevoked",
            CourtEvent::ContemptRecorded { .. } => "ContemptRecorded",
            CourtEvent::Superseded { .. } => "Superseded",
            CourtEvent::HonourConferred { .. } => "HonourConferred",
        }
    }

    /// Canonical bytes (serde_json, tagged). Deterministic across nodes.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Is this act **authoritative** — something the whole nation's court should hold,
    /// regardless of which replica it was filed against — so it belongs on the
    /// `/sigil/g2/court` gossip topic? Excludes [`CourtEvent::JusticeAppointed`], which each
    /// node derives deterministically by seeding its own bench from its own block archive at
    /// boot: gossiping those would sum the benches (5 + 3 = 8 justices), not converge them.
    /// Every other act — rulings, appeals, precedents, promotions, exams, credentials,
    /// disclosure orders/exports, contempt, supersessions and honours — is a real decision
    /// that must reach every replica.
    pub fn is_gossipable(&self) -> bool {
        !matches!(self, CourtEvent::JusticeAppointed { .. })
    }

    /// Order-independent content id for cross-node convergence: BLAKE3 over the chain height
    /// the origin recorded the act at, joined with its canonical bytes. Two replicas that hold
    /// the same (height, act) compute the same id — the id carries NO seq/prev, so it does not
    /// depend on the local docket order. This is the leaf of [`Docket::set_root`] and the dedup
    /// key for [`Docket::apply_external`].
    pub fn gossip_id(&self, height: u64) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/gossip-id");
        h.update(&height.to_le_bytes());
        h.update(&self.encode());
        *h.finalize().as_bytes()
    }

    /// The case this event belongs to, if any (for the per-case index).
    pub fn case(&self) -> Option<CaseId> {
        match self {
            CourtEvent::CaseFiled { case, .. }
            | CourtEvent::EvidenceAdmitted { case, .. }
            | CourtEvent::HearingHeld { case, .. }
            | CourtEvent::RulingIssued { case, .. }
            | CourtEvent::AppealFiled { case, .. }
            | CourtEvent::AppealDecided { case, .. }
            | CourtEvent::Recused { case, .. }
            | CourtEvent::ContemptRecorded { case, .. } => Some(*case),
            _ => None,
        }
    }

    /// Every wallet this event names (for the per-wallet index).
    pub fn wallets(&self) -> Vec<WalletId> {
        match self {
            CourtEvent::CaseFiled { plaintiff, defendant, .. } => {
                let mut v = vec![*plaintiff];
                if let Some(d) = defendant { v.push(*d); }
                v
            }
            CourtEvent::HearingHeld { panel, presiding, .. } => {
                let mut v = panel.clone();
                v.push(*presiding);
                v
            }
            CourtEvent::AppealFiled { appellant, .. } => vec![*appellant],
            CourtEvent::JusticeAppointed { justice, .. }
            | CourtEvent::Promoted { justice, .. }
            | CourtEvent::Recused { justice, .. } => vec![*justice],
            CourtEvent::ExamSat { candidate, .. } => vec![*candidate],
            CourtEvent::CredentialConferred { holder, .. } => vec![*holder],
            CourtEvent::ContemptRecorded { wallet, .. } => vec![*wallet],
            CourtEvent::HonourConferred { recipient, conferred_by, .. } => vec![*recipient, *conferred_by],
            _ => Vec::new(),
        }
    }

    /// The disclosure order this event concerns, if any.
    pub fn order(&self) -> Option<OrderId> {
        match self {
            CourtEvent::DisclosureOrdered { order, .. }
            | CourtEvent::DisclosureExported { order, .. }
            | CourtEvent::DisclosureRefused { order, .. }
            | CourtEvent::DisclosureRevoked { order, .. } => Some(*order),
            _ => None,
        }
    }
}

/// One docket line: sequence number, chain height it was recorded at, the previous entry's leaf
/// (the chain link), this entry's leaf, and the event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocketEntry {
    pub seq: u64,
    pub height: u64,
    pub prev: [u8; 32],
    pub leaf: [u8; 32],
    pub event: CourtEvent,
}

impl DocketEntry {
    /// leaf = BLAKE3("sigil-court/docket" ‖ prev ‖ seq ‖ height ‖ encode(event)).
    pub fn compute_leaf(prev: &[u8; 32], seq: u64, height: u64, event: &CourtEvent) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/docket");
        h.update(prev);
        h.update(&seq.to_le_bytes());
        h.update(&height.to_le_bytes());
        h.update(&event.encode());
        *h.finalize().as_bytes()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DocketError {
    #[error("docket chain broken at seq {seq}: {what}")]
    ChainBroken { seq: u64, what: &'static str },
    #[error("seq {0} does not exist")]
    NoSuchEntry(u64),
}

/// The append-only docket. `Default` = the genesis (empty) docket whose head is all-zero.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Docket {
    entries: Vec<DocketEntry>,
    #[serde(skip)]
    by_case: BTreeMap<CaseId, Vec<u64>>,
    #[serde(skip)]
    by_wallet: BTreeMap<WalletId, Vec<u64>>,
    #[serde(skip)]
    by_order: BTreeMap<OrderId, Vec<u64>>,
}

impl Docket {
    pub fn new() -> Self { Self::default() }

    /// Append an event at chain `height`. Returns its seq. Never fails: an append-only log has
    /// no invalid append — validity of the ACT is the court's job before it reaches here.
    pub fn append(&mut self, height: u64, event: CourtEvent) -> u64 {
        let seq = self.entries.len() as u64;
        let prev = self.head();
        let leaf = DocketEntry::compute_leaf(&prev, seq, height, &event);
        if let Some(c) = event.case() { self.by_case.entry(c).or_default().push(seq); }
        if let Some(o) = event.order() { self.by_order.entry(o).or_default().push(seq); }
        for w in event.wallets() { self.by_wallet.entry(w).or_default().push(seq); }
        self.entries.push(DocketEntry { seq, height, prev, leaf, event });
        seq
    }

    /// Chain head — the last entry's leaf (all-zero when empty).
    pub fn head(&self) -> [u8; 32] {
        self.entries.last().map(|e| e.leaf).unwrap_or([0u8; 32])
    }

    /// Balanced Merkle root over all entry leaves (all-zero when empty).
    pub fn root(&self) -> [u8; 32] {
        let leaves: Vec<[u8; 32]> = self.entries.iter().map(|e| e.leaf).collect();
        merkle::root(&leaves)
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
    pub fn entries(&self) -> &[DocketEntry] { &self.entries }
    pub fn get(&self, seq: u64) -> Option<&DocketEntry> { self.entries.get(seq as usize) }

    /// Inclusion proof for entry `seq` against [`Docket::root`].
    pub fn prove(&self, seq: u64) -> Option<MerkleProof> {
        let leaves: Vec<[u8; 32]> = self.entries.iter().map(|e| e.leaf).collect();
        merkle::prove(&leaves, seq as usize)
    }

    /// Verify a docket inclusion proof.
    pub fn verify(leaf: [u8; 32], proof: &MerkleProof, root: [u8; 32]) -> bool {
        merkle::verify(leaf, proof, root)
    }

    /// Recompute every leaf and every link. `Ok` iff nothing was altered in place.
    pub fn verify_chain(&self) -> Result<(), DocketError> {
        let mut prev = [0u8; 32];
        for (i, e) in self.entries.iter().enumerate() {
            if e.seq != i as u64 { return Err(DocketError::ChainBroken { seq: i as u64, what: "seq" }); }
            if e.prev != prev { return Err(DocketError::ChainBroken { seq: e.seq, what: "prev link" }); }
            let leaf = DocketEntry::compute_leaf(&prev, e.seq, e.height, &e.event);
            if leaf != e.leaf { return Err(DocketError::ChainBroken { seq: e.seq, what: "leaf" }); }
            prev = leaf;
        }
        Ok(())
    }

    /// Rebuild the in-memory indexes (after deserializing).
    pub fn reindex(&mut self) {
        self.by_case.clear();
        self.by_wallet.clear();
        self.by_order.clear();
        for e in &self.entries {
            if let Some(c) = e.event.case() { self.by_case.entry(c).or_default().push(e.seq); }
            if let Some(o) = e.event.order() { self.by_order.entry(o).or_default().push(e.seq); }
            for w in e.event.wallets() { self.by_wallet.entry(w).or_default().push(e.seq); }
        }
    }

    pub fn by_case(&self, case: &CaseId) -> Vec<&DocketEntry> {
        self.by_case.get(case).map(|v| v.iter().map(|s| &self.entries[*s as usize]).collect()).unwrap_or_default()
    }
    pub fn by_wallet(&self, w: &WalletId) -> Vec<&DocketEntry> {
        self.by_wallet.get(w).map(|v| v.iter().map(|s| &self.entries[*s as usize]).collect()).unwrap_or_default()
    }
    pub fn by_order(&self, o: &OrderId) -> Vec<&DocketEntry> {
        self.by_order.get(o).map(|v| v.iter().map(|s| &self.entries[*s as usize]).collect()).unwrap_or_default()
    }
    pub fn by_tag(&self, tag: u8) -> Vec<&DocketEntry> {
        self.entries.iter().filter(|e| e.event.tag() == tag).collect()
    }

    /// The set of gossip ids currently held (authoritative acts only), sorted. The basis of
    /// [`Docket::set_root`]; also the "have" set a node advertises so peers send only what is missing.
    pub fn gossip_ids(&self) -> Vec<[u8; 32]> {
        let mut ids: Vec<[u8; 32]> = self.entries.iter()
            .filter(|e| e.event.is_gossipable())
            .map(|e| e.event.gossip_id(e.height))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// **Order-independent** commitment over the authoritative acts: a balanced Merkle root over
    /// the SORTED gossip ids. Unlike [`Docket::root`]/[`Docket::head`] (which fix insertion order),
    /// two replicas that hold the same SET of acts commit to the same `set_root` no matter what
    /// order the acts arrived in. This is the value two nodes must match to be "one court", and
    /// what the `/sigil/g2/court` gossip topic drives to agreement.
    pub fn set_root(&self) -> [u8; 32] {
        merkle::root(&self.gossip_ids())
    }

    /// Does the docket already hold this authoritative act (by gossip id)?
    pub fn contains_gossip(&self, height: u64, event: &CourtEvent) -> bool {
        let id = event.gossip_id(height);
        self.entries.iter().any(|e| e.event.is_gossipable() && e.event.gossip_id(e.height) == id)
    }

    /// Apply an act received from a peer. Appends it (preserving the origin's `height`) iff it is
    /// gossipable and not already held. Returns `true` when it was NEW — so the caller knows to
    /// re-broadcast and log. Idempotent: a duplicate is a no-op returning `false`. Never fails; an
    /// append-only log has no invalid append, and the caller has already checked authenticity.
    pub fn apply_external(&mut self, height: u64, event: CourtEvent) -> bool {
        if !event.is_gossipable() || self.contains_gossip(height, &event) {
            return false;
        }
        self.append(height, event);
        true
    }

    /// Count per event name — the docket's own statistics.
    pub fn histogram(&self) -> BTreeMap<&'static str, u32> {
        let mut m = BTreeMap::new();
        for e in &self.entries { *m.entry(e.event.name()).or_insert(0) += 1; }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(b: u8) -> WalletId { [b; 32] }

    #[test]
    fn chain_and_root_move_on_every_append_and_verify() {
        let mut d = Docket::new();
        assert_eq!(d.head(), [0u8; 32]);
        assert_eq!(d.root(), [0u8; 32]);
        let s0 = d.append(1, CourtEvent::JusticeAppointed { justice: w(1), rank: Rank::Justice });
        let h1 = d.head();
        let s1 = d.append(2, CourtEvent::CaseFiled { case: [7; 32], plaintiff: w(2), defendant: Some(w(3)), article: Article::PrivacyByDefault, claim: "x".into() });
        assert_eq!((s0, s1), (0, 1));
        assert_ne!(d.head(), h1);
        assert_eq!(d.get(1).unwrap().prev, h1);
        d.verify_chain().unwrap();
        let p = d.prove(1).unwrap();
        assert!(Docket::verify(d.get(1).unwrap().leaf, &p, d.root()));
        assert_eq!(d.by_case(&[7; 32]).len(), 1);
        assert_eq!(d.by_wallet(&w(3)).len(), 1);
        assert_eq!(d.by_wallet(&w(1)).len(), 1);
    }

    #[test]
    fn in_place_tamper_is_detected() {
        let mut d = Docket::new();
        d.append(1, CourtEvent::JusticeAppointed { justice: w(1), rank: Rank::Clerk });
        d.append(1, CourtEvent::JusticeAppointed { justice: w(2), rank: Rank::Clerk });
        // Reach in (there is no public mutator) and rewrite history.
        d.entries[0].event = CourtEvent::JusticeAppointed { justice: w(1), rank: Rank::ChiefJustice };
        assert_eq!(d.verify_chain(), Err(DocketError::ChainBroken { seq: 0, what: "leaf" }));
    }

    #[test]
    fn serde_roundtrip_keeps_chain_and_reindexes() {
        let mut d = Docket::new();
        d.append(3, CourtEvent::ExamSat { candidate: w(9), course: Course::Evidence, score_bps: 8000, passed: true });
        let js = serde_json::to_string(&d).unwrap();
        let mut back: Docket = serde_json::from_str(&js).unwrap();
        back.verify_chain().unwrap();
        assert!(back.by_wallet(&w(9)).is_empty(), "indexes are skipped in serde");
        back.reindex();
        assert_eq!(back.by_wallet(&w(9)).len(), 1);
        assert_eq!(back.root(), d.root());
    }
    #[test]
    fn set_root_converges_regardless_of_arrival_order() {
        // Three authoritative acts, recorded at fixed heights.
        let a = CourtEvent::HonourConferred { order: "Elephant".into(), rank: "Aeresborger".into(), recipient: w(7), citation: "for the chain".into(), conferred_by: w(1), approvals: 4, operator_cosigned: true };
        let b = CourtEvent::ContemptRecorded { wallet: w(9), case: [3; 32], reason: "no-show".into() };
        let c = CourtEvent::RulingIssued { case: [5; 32], ruling: [6; 32], verdict: Verdict::Upheld, article: Article::PrivacyByDefault, votes_for: 3, votes_against: 0, cites: vec![] };

        // Node 1 records a,b,c ; node 2 receives them c,a,b (different order) plus its own local bench.
        let mut d1 = Docket::new();
        d1.append(100, a.clone()); d1.append(101, b.clone()); d1.append(102, c.clone());

        let mut d2 = Docket::new();
        d2.append(1, CourtEvent::JusticeAppointed { justice: w(50), rank: Rank::Justice }); // node-local, must NOT affect set_root
        assert!(d2.apply_external(102, c.clone()));
        assert!(d2.apply_external(100, a.clone()));
        assert!(d2.apply_external(101, b.clone()));

        // Heads/roots differ (order + the local bench entry); the SET root converges.
        assert_ne!(d1.head(), d2.head(), "insertion-ordered head is expectedly local");
        assert_eq!(d1.set_root(), d2.set_root(), "same authoritative SET => same set_root");
    }

    #[test]
    fn apply_external_is_idempotent_and_excludes_bench() {
        let mut d = Docket::new();
        let h = CourtEvent::HonourConferred { order: "Elephant".into(), rank: "Aeresborger".into(), recipient: w(7), citation: "x".into(), conferred_by: w(1), approvals: 4, operator_cosigned: true };
        assert!(d.apply_external(100, h.clone()), "first apply is new");
        let r1 = d.set_root();
        assert!(!d.apply_external(100, h.clone()), "duplicate is a no-op");
        assert_eq!(d.set_root(), r1, "duplicate does not move set_root");
        // A bench appointment is not gossipable and must be refused by apply_external.
        assert!(!d.apply_external(5, CourtEvent::JusticeAppointed { justice: w(2), rank: Rank::Justice }));
        assert_eq!(d.set_root(), r1, "non-gossipable act never enters the set");
    }

}
