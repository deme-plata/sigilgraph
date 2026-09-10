//! sigil-court — the SIGIL Nation SUPREME COURT, version 3. **Code is law.**
//!
//! The highest bench of the nation, as one pure-function object:
//!
//! ```text
//!   constitution (12 Articles, hashed)            → every refusal names its Article
//!   docket   — hash-chained, Merkle-rooted log    → the court's event database
//!   bench    — exams + deeds + quorum promotion   → the highest place to be educated/promoted
//!   register — cases, rulings, precedent, appeal  → majority rules, 2/3 en banc overrules
//!   orders   — disclosure to foreign authorities  → minimized, inclusion-proven, SQIsign-sealed
//!   court_root = BLAKE3(constitution ‖ docket ‖ bench ‖ cases ‖ precedents ‖ orders)
//! ```
//!
//! No I/O, no clock — every act takes the chain height and unix time it happens at, so every node
//! replays the same court to the same root. `chain::commit_court_root` puts that root in
//! `contract_state_root`, exactly as the Nation's root is committed.

#![forbid(unsafe_code)]

pub mod bench;
pub mod chain;
pub mod constitution;
pub mod disclosure;
pub mod docket;
pub mod merkle;
pub mod ruling;
pub mod science;

use std::collections::{BTreeMap, BTreeSet};

use sigil_events::{HonorPolicy, MerkleProof, SigilEvent, SigilOrder};
use sigil_state::WalletId;
use sigil_university::UniversityRegistry;

pub use bench::{Bench, BenchError, Course, Credential, Justice, Rank, Requirements, PASS_BPS};
pub use constitution::{constitution_hash, constitution_hash_hex, Article, CONSTITUTION_VERSION};
pub use disclosure::{
    bank_scope, tax_scope, verify_packet, BlockEvents, CourtKeys, DisclosedRecord, DisclosureError, DisclosureOrder,
    DisclosurePacket, DisclosureRequest, Jurisdiction, KeyKind, Minimization, Purpose, Scope, Seal, Summary, Verified,
    ViewingGrant, PACKET_SCHEMA,
};
pub use sigil_events::SigilOrder as Order;
pub use docket::{CaseId, CourtEvent, Docket, DocketEntry, DocketError, OrderId, RulingHash};
pub use ruling::{AppealOutcome, Case, CaseStatus, Evidence, Precedent, Register, Ruling, RulingError, Verdict};

/// Every way the court refuses. Art. X: fail loud, name the Article where one applies.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CourtError {
    #[error("{article}: {reason}")]
    Unconstitutional { article: Article, reason: String },
    #[error(transparent)]
    Bench(#[from] BenchError),
    #[error(transparent)]
    Ruling(#[from] RulingError),
    #[error(transparent)]
    Disclosure(#[from] DisclosureError),
    #[error("no such order")]
    NoSuchOrder,
    #[error("Art. II: a disclosure order needs a panel majority of at least three Magistrates ({votes_for} of {panel})")]
    OrderNotCarried { votes_for: u32, panel: u32 },
}

/// The Supreme Court.
pub struct SupremeCourt {
    docket: Docket,
    bench: Bench,
    register: Register,
    orders: BTreeMap<OrderId, DisclosureOrder>,
    exported: BTreeMap<OrderId, [u8; 32]>,
    revoked: BTreeSet<OrderId>,
    keys: CourtKeys,
}

impl SupremeCourt {
    /// A court with its SQIsign seal derived from `seed` (the Clerk of Court's key).
    pub fn from_seed(seed: &[u8; 32]) -> Self { Self::with_keys(CourtKeys::from_seed(seed)) }

    pub fn with_keys(keys: CourtKeys) -> Self {
        Self { docket: Docket::new(), bench: Bench::new(), register: Register::new(), orders: BTreeMap::new(), exported: BTreeMap::new(), revoked: BTreeSet::new(), keys }
    }

    pub fn docket(&self) -> &Docket { &self.docket }



    pub fn bench(&self) -> &Bench { &self.bench }
    pub fn register(&self) -> &Register { &self.register }
    pub fn orders(&self) -> &BTreeMap<OrderId, DisclosureOrder> { &self.orders }
    /// The court's SQIsign public key — what a foreign verifier pins.
    pub fn public_key(&self) -> &[u8] { &self.keys.pk }

    // ─────────────────────────── the bench: education + promotion ───────────────────────────

    pub fn appoint(&mut self, justice: WalletId, rank: Rank, height: u64) -> Result<(), CourtError> {
        self.bench.appoint(justice, rank, height)?;
        self.docket.append(height, CourtEvent::JusticeAppointed { justice, rank });
        Ok(())
    }

    /// Sit an exam. Every attempt is on the record, pass or fail.
    pub fn sit_exam(&mut self, candidate: WalletId, course: Course, score_bps: u16, height: u64) -> Result<bool, CourtError> {
        let (passed, conferred) = self.bench.sit_exam(&candidate, course, score_bps)?;
        self.docket.append(height, CourtEvent::ExamSat { candidate, course, score_bps, passed });
        for c in conferred {
            self.docket.append(height, CourtEvent::CredentialConferred { holder: candidate, credential: c, source: format!("exam:{}", course.name()) });
        }
        Ok(passed)
    }

    /// A SIGIL University graduate, per the registrar, earns a court credential.
    pub fn admit_graduate(&mut self, holder: WalletId, registry: &UniversityRegistry, height: u64) -> Result<(), CourtError> {
        let c = self.bench.admit_graduate(&holder, registry)?;
        self.docket.append(height, CourtEvent::CredentialConferred { holder, credential: c, source: "sigil-university registrar".into() });
        Ok(())
    }

    /// Art. VIII — promote by credentials, deeds and quorum.
    pub fn promote(&mut self, candidate: WalletId, to: Rank, voters: &[WalletId], height: u64) -> Result<(), CourtError> {
        let (from, to, votes_for, electorate) = self.bench.promote(&candidate, to, voters)?;
        self.docket.append(height, CourtEvent::Promoted { justice: candidate, from, to, votes_for, electorate });
        Ok(())
    }

    /// Confer an **Order of the nation** — æresborger. The gate is
    /// [`sigil_events::HonorPolicy`], not this method: `SigilEvent::confer_honor` returns `None`
    /// unless the rank is valid for the order AND the conferral quorum is met. The Elephant needs
    /// the operator's co-signature AND at least three approvals, so the supreme honour of this
    /// nation cannot be handed out by one person — including the operator. Honour over power.
    ///
    /// Returns the chain event to emit, so the honour lands in the block's event log and not only
    /// on the court's docket. The recipient must already hold a seat; the court can only decorate
    /// someone it can name.
    #[allow(clippy::too_many_arguments)]
    pub fn confer_honour(
        &mut self,
        order: SigilOrder,
        rank: impl Into<String>,
        recipient: WalletId,
        citation: impl Into<String>,
        conferred_by: WalletId,
        approvals: usize,
        operator_cosigned: bool,
        height: u64,
    ) -> Result<SigilEvent, CourtError> {
        let (rank, citation) = (rank.into(), citation.into());
        let ev = SigilEvent::confer_honor(
            order.clone(), rank.clone(), recipient, citation.clone(), conferred_by, approvals, operator_cosigned,
        )
        .ok_or(CourtError::Unconstitutional {
            article: Article::PromotionByDeeds,
            reason: format!(
                "the conferral rule refuses this: {:?} needs {} distinct approvals{} and a rank valid for the order (got {} approvals, operator_cosigned={}, rank={:?})",
                order,
                HonorPolicy::quorum_required(&order),
                if matches!(order, SigilOrder::Elefantordenen) { " AND the operator's co-signature" } else { " OR the operator's co-signature" },
                approvals, operator_cosigned, rank
            ),
        })?;
        self.bench.record_honour(&recipient)?;
        self.docket.append(height, CourtEvent::HonourConferred {
            order: format!("{order:?}"), rank, recipient, citation, conferred_by,
            approvals: approvals as u32, operator_cosigned,
        });
        Ok(ev)
    }

    pub fn recuse(&mut self, justice: WalletId, case: CaseId, reason: impl Into<String>, height: u64) -> Result<(), CourtError> {
        self.bench.recuse(&justice, case)?;
        self.docket.append(height, CourtEvent::Recused { justice, case, reason: reason.into() });
        Ok(())
    }

    // ─────────────────────────── cases, rulings, appeals ───────────────────────────

    pub fn file_case(&mut self, plaintiff: WalletId, defendant: Option<WalletId>, article: Article, claim: impl Into<String>, height: u64) -> CaseId {
        let claim = claim.into();
        let case = self.register.file(plaintiff, defendant, article, claim.clone(), height);
        self.docket.append(height, CourtEvent::CaseFiled { case, plaintiff, defendant, article, claim });
        case
    }

    /// Art. V — evidence enters the record only with a verified inclusion proof.
    pub fn admit_evidence(&mut self, case: CaseId, event: SigilEvent, event_height: u64, index: u32, proof: MerkleProof, event_log_root: [u8; 32], height: u64) -> Result<[u8; 32], CourtError> {
        let tag = event.tag();
        let leaf = self.register.admit_evidence(&case, event, event_height, index, proof, event_log_root)?;
        self.docket.append(height, CourtEvent::EvidenceAdmitted { case, height: event_height, index, leaf, tag, proven: true });
        Ok(leaf)
    }

    /// Seat a deterministic panel of `size` (≥3) and hold the hearing. Returns the panel.
    pub fn hear(&mut self, case: CaseId, size: usize, height: u64) -> Result<Vec<WalletId>, CourtError> {
        let panel = self.bench.select_panel(&case, size.max(3))?;
        self.register.hear(&case, panel.clone())?;
        let presiding = self.bench.presiding(&panel).expect("non-empty panel");
        self.bench.record_panel(&panel);
        self.docket.append(height, CourtEvent::HearingHeld { case, panel: panel.clone(), presiding });
        Ok(panel)
    }

    /// The panel rules. Non-dismissals set precedent (Art. VII).
    pub fn rule(&mut self, case: CaseId, verdict: Verdict, holding: impl Into<String>, votes: &[(WalletId, bool)], cites: Vec<RulingHash>, height: u64) -> Result<RulingHash, CourtError> {
        let holding = holding.into();
        let (ruling, sets_precedent) = self.register.rule(&case, verdict, holding.clone(), votes, cites.clone(), height)?;
        let r = self.register.ruling(&ruling).expect("just inserted");
        let (article, votes_for, votes_against) = (r.article, r.votes_for, r.votes_against);
        self.docket.append(height, CourtEvent::RulingIssued { case, ruling, verdict, article, votes_for, votes_against, cites });
        if sets_precedent {
            self.docket.append(height, CourtEvent::PrecedentSet { ruling, article, holding });
        }
        Ok(ruling)
    }

    /// Art. VI — a party appeals, once.
    pub fn appeal(&mut self, case: CaseId, appellant: WalletId, height: u64) -> Result<RulingHash, CourtError> {
        let of_ruling = self.register.appeal(&case, &appellant)?;
        self.docket.append(height, CourtEvent::AppealFiled { case, appellant, of_ruling });
        Ok(of_ruling)
    }

    /// The full bench decides en banc. `votes` = every en banc member's vote FOR REVERSAL.
    pub fn decide_appeal(&mut self, case: CaseId, votes: &[(WalletId, bool)], holding: impl Into<String>, height: u64) -> Result<Verdict, CourtError> {
        let en_banc = self.bench.en_banc(&case);
        let out = self.register.decide_appeal(&case, en_banc.clone(), votes, holding, height)?;
        self.bench.record_panel(&en_banc);
        if out.verdict == Verdict::Reversed { self.bench.record_reversed(&out.original_panel); } else { self.bench.record_upheld(&out.original_panel); }
        self.docket.append(height, CourtEvent::AppealDecided { case, ruling: out.ruling, verdict: out.verdict, votes_for: out.votes_for_reversal, bench: out.bench });
        if out.precedent_overruled {
            self.docket.append(height, CourtEvent::PrecedentOverruled { ruling: out.original, by: out.ruling, votes_for: out.votes_for_reversal, bench: out.bench });
        }
        Ok(out.verdict)
    }

    pub fn record_contempt(&mut self, wallet: WalletId, case: CaseId, reason: impl Into<String>, height: u64) {
        self.docket.append(height, CourtEvent::ContemptRecorded { wallet, case, reason: reason.into() });
    }

    /// Art. IX — mark a docket entry superseded. The entry itself never moves.
    pub fn supersede(&mut self, seq: u64, reason: impl Into<String>, height: u64) -> Result<(), CourtError> {
        if self.docket.get(seq).is_none() { return Err(CourtError::Unconstitutional { article: Article::SupersedeNeverDelete, reason: format!("no docket entry {seq} to supersede") }); }
        self.docket.append(height, CourtEvent::Superseded { seq, reason: reason.into() });
        Ok(())
    }

    // ─────────────────────────── disclosure to foreign authorities ───────────────────────────

    /// Art. II — issue a disclosure order. Refuses a spend key (Art. III) and records the refusal;
    /// needs a deterministic panel of ≥3 sitting Magistrates with a majority FOR; a `CourtOrder`
    /// purpose needs a decided case. `now_ts` is the issue time; expiry = requested_ts + ttl.
    pub fn order_disclosure(&mut self, request: DisclosureRequest, case: Option<CaseId>, votes: &[(WalletId, bool)], height: u64, now_ts: u64) -> Result<OrderId, CourtError> {
        let id = disclosure::order_id(&request, height, now_ts);
        if request.key_kind == Some(KeyKind::Spend) {
            let reason = "a spend key is not disclosable by any Order, panel, or vote".to_string();
            self.docket.append(height, CourtEvent::DisclosureRefused { order: id, article: Article::SpendKeysNeverLeave, reason: reason.clone() });
            return Err(CourtError::Unconstitutional { article: Article::SpendKeysNeverLeave, reason });
        }
        if request.scope.to_height < request.scope.from_height {
            return Err(CourtError::Disclosure(DisclosureError::EmptyScope));
        }
        if request.purpose == Purpose::CourtOrder {
            let ok = case.and_then(|c| self.register.get(&c)).map(|c| matches!(c.status, CaseStatus::Decided | CaseStatus::Final)).unwrap_or(false);
            if !ok {
                let reason = "a CourtOrder disclosure needs a decided case".to_string();
                self.docket.append(height, CourtEvent::DisclosureRefused { order: id, article: Article::NoDisclosureWithoutOrder, reason: reason.clone() });
                return Err(CourtError::Unconstitutional { article: Article::NoDisclosureWithoutOrder, reason });
            }
        }
        // Panel: the deterministic panel for this order id (≥3 Magistrates), majority FOR.
        let panel = self.bench.select_panel(&id, 3)?;
        let mut seen = BTreeSet::new();
        let mut votes_for = 0u32;
        for (w, support) in votes {
            if !panel.contains(w) || !seen.insert(*w) { return Err(CourtError::Ruling(RulingError::StrangerVote(hex::encode(w)))); }
            if *support { votes_for += 1; }
        }
        if seen.len() != panel.len() || votes_for * 2 <= panel.len() as u32 {
            return Err(CourtError::OrderNotCarried { votes_for, panel: panel.len() as u32 });
        }
        let expires_ts = request.requested_ts.saturating_add(request.ttl_secs);
        let order = DisclosureOrder {
            id, minimization: Minimization::for_purpose(request.purpose), request, case, issued_height: height, issued_ts: now_ts, expires_ts, panel: panel.clone(), votes_for,
        };
        self.docket.append(height, CourtEvent::DisclosureOrdered {
            order: id,
            jurisdiction: order.request.jurisdiction.label(),
            purpose: order.request.purpose,
            subjects: order.request.scope.subjects.len() as u32,
            from_height: order.request.scope.from_height,
            to_height: order.request.scope.to_height,
            expires_ts,
            votes_for,
            panel: panel.len() as u32,
        });
        self.orders.insert(id, order);
        Ok(id)
    }

    /// Export the packet for an order. The court root sealed is the root BEFORE this export is
    /// recorded (so the packet commits to the state that authorized it).
    pub fn export_disclosure(&mut self, order: OrderId, blocks: &[BlockEvents], viewing_keys: &[(WalletId, [u8; 32])], height: u64, now_ts: u64) -> Result<DisclosurePacket, CourtError> {
        if self.revoked.contains(&order) { return Err(CourtError::Disclosure(DisclosureError::Revoked)); }
        let o = self.orders.get(&order).ok_or(CourtError::NoSuchOrder)?.clone();
        let court_root = self.court_root();
        let packet = disclosure::export(&o, blocks, viewing_keys, court_root, &self.keys, now_ts)?;
        self.exported.insert(order, packet.seal.body.packet_root);
        self.docket.append(height, CourtEvent::DisclosureExported {
            order,
            packet_root: packet.seal.body.packet_root,
            records: packet.records.len() as u32,
            redacted_fields: packet.summary.redacted_fields,
            viewing_grants: packet.viewing_grants.len() as u32,
        });
        Ok(packet)
    }

    pub fn revoke_disclosure(&mut self, order: OrderId, reason: impl Into<String>, height: u64) -> Result<(), CourtError> {
        if !self.orders.contains_key(&order) { return Err(CourtError::NoSuchOrder); }
        self.revoked.insert(order);
        self.docket.append(height, CourtEvent::DisclosureRevoked { order, reason: reason.into() });
        Ok(())
    }

    // ─────────────────────────── the root ───────────────────────────

    fn orders_root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/orders");
        for (id, o) in &self.orders {
            h.update(id);
            h.update(&o.expires_ts.to_le_bytes());
            h.update(&o.votes_for.to_le_bytes());
            h.update(self.exported.get(id).unwrap_or(&[0u8; 32]));
            h.update(&[self.revoked.contains(id) as u8]);
        }
        *h.finalize().as_bytes()
    }

    /// The ONE committed root over the whole court.
    pub fn court_root(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-court/root/v3");
        h.update(&constitution_hash());
        h.update(&self.docket.root());
        h.update(&self.bench.root());
        h.update(&self.register.cases_root());
        h.update(&self.register.precedent_root());
        h.update(&self.orders_root());
        *h.finalize().as_bytes()
    }
    pub fn court_root_hex(&self) -> String { hex::encode(self.court_root()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_events::prove_inclusion;
    use sigil_state::NATIVE;

    fn w(b: u8) -> WalletId { [b; 32] }

    fn seated() -> SupremeCourt {
        let mut c = SupremeCourt::from_seed(&[11; 32]);
        c.appoint(w(1), Rank::ChiefJustice, 1).unwrap();
        c.appoint(w(2), Rank::Justice, 1).unwrap();
        c.appoint(w(3), Rank::Justice, 1).unwrap();
        c.appoint(w(4), Rank::Magistrate, 1).unwrap();
        c.appoint(w(5), Rank::Magistrate, 1).unwrap();
        c
    }

    fn chain() -> Vec<BlockEvents> {
        vec![
            BlockEvents::seal(10, vec![
                SigilEvent::MintReward { miner: w(7), height: 10, amount: 500 },
                SigilEvent::Send { from: w(7), to: w(8), amount: 120, token: NATIVE, fee: 3 },
            ]),
            BlockEvents::seal(11, vec![SigilEvent::WelfareClaimed { citizen: w(7), amount: 25 }]),
        ]
    }

    fn all_for(panel: &[WalletId]) -> Vec<(WalletId, bool)> { panel.iter().map(|p| (*p, true)).collect() }

    #[test]
    fn full_story_case_to_sealed_packet_and_every_act_is_on_the_docket() {
        let mut c = seated();
        let ch = chain();
        let case = c.file_case(w(30), Some(w(7)), Article::Minimization, "SKAT v. wallet 07: annual statement", 12);
        // evidence: proven → in; unproven → refused and NOT on the docket
        let p = prove_inclusion(&ch[0].events, 1).unwrap();
        c.admit_evidence(case, ch[0].events[1].clone(), 10, 1, p.clone(), ch[0].event_log_root, 12).unwrap();
        assert!(matches!(c.admit_evidence(case, ch[0].events[0].clone(), 10, 1, p, ch[0].event_log_root, 12), Err(CourtError::Ruling(RulingError::NotProven { .. }))));
        let panel = c.hear(case, 3, 13).unwrap();
        let ruling = c.rule(case, Verdict::Upheld, "disclose the least that answers the purpose", &all_for(&panel), vec![], 14).unwrap();
        assert!(c.register().precedent(&ruling).unwrap().is_live());

        // spend key → Art. III refusal, recorded
        let mut req = DisclosureRequest { jurisdiction: Jurisdiction::new("DK", "SKAT"), purpose: Purpose::Tax, scope: tax_scope(w(7), 0, 100), key_kind: Some(KeyKind::Spend), reason: "årsopgørelse".into(), requested_ts: 1_000, ttl_secs: 3_600 };
        let err = c.order_disclosure(req.clone(), Some(case), &[], 15, 1_000).unwrap_err();
        assert!(matches!(err, CourtError::Unconstitutional { article: Article::SpendKeysNeverLeave, .. }));
        assert_eq!(c.docket().by_tag(15).len(), 1);

        // viewing key → order, majority of a deterministic 3-Magistrate panel
        req.key_kind = Some(KeyKind::Viewing);
        let id = disclosure::order_id(&req, 15, 1_000);
        let panel = c.bench().select_panel(&id, 3).unwrap();
        let mut votes = all_for(&panel);
        votes[0].1 = false;
        let id2 = c.order_disclosure(req.clone(), Some(case), &votes, 15, 1_000).unwrap();
        assert_eq!(id, id2);
        let pkt = c.export_disclosure(id, &ch, &[(w(7), [0xAB; 32])], 16, 2_000).unwrap();
        assert_eq!(pkt.records.len(), 3);
        let roots: BTreeMap<u64, [u8; 32]> = ch.iter().map(|b| (b.height, b.event_log_root)).collect();
        let v = verify_packet(&pkt, &roots, c.public_key(), 3_000).unwrap();
        assert_eq!(v.records_proven, 3);
        // the seal commits the court root that authorized it
        assert_ne!(pkt.seal.body.court_root, c.court_root(), "root moved once the export was recorded");

        // revoke → further exports refused
        c.revoke_disclosure(id, "authority withdrew", 17).unwrap();
        assert_eq!(c.export_disclosure(id, &ch, &[], 18, 2_500), Err(CourtError::Disclosure(DisclosureError::Revoked)));

        // appeal en banc: 3 Justices (w1,w2,w3); 2 of 3 reverse → case reversed, precedent stands (2 < ceil(6/3)=2? no: 2 >= 2 → overruled)
        c.appeal(case, w(7), 19).unwrap();
        let v = c.decide_appeal(case, &[(w(1), true), (w(2), true), (w(3), false)], "reversed en banc", 20).unwrap();
        assert_eq!(v, Verdict::Reversed);
        assert!(!c.register().precedent(&ruling).unwrap().is_live(), "2 of 3 is two thirds: overruled");

        let hist = c.docket().histogram();
        for k in ["CaseFiled", "EvidenceAdmitted", "HearingHeld", "RulingIssued", "PrecedentSet", "DisclosureRefused", "DisclosureOrdered", "DisclosureExported", "DisclosureRevoked", "AppealFiled", "AppealDecided", "PrecedentOverruled", "JusticeAppointed"] {
            assert!(hist.contains_key(k), "{k} missing from the docket");
        }
        c.docket().verify_chain().unwrap();
    }

    #[test]
    fn order_needs_a_carried_panel_and_court_order_needs_a_decided_case() {
        let mut c = seated();
        let req = DisclosureRequest { jurisdiction: Jurisdiction::new("EU", "AMLA"), purpose: Purpose::Aml, scope: tax_scope(w(7), 0, 10), key_kind: None, reason: "r".into(), requested_ts: 0, ttl_secs: 10 };
        let id = disclosure::order_id(&req, 5, 0);
        let panel = c.bench().select_panel(&id, 3).unwrap();
        let mut votes = all_for(&panel);
        votes[0].1 = false;
        votes[1].1 = false;
        assert_eq!(c.order_disclosure(req.clone(), None, &votes, 5, 0), Err(CourtError::OrderNotCarried { votes_for: 1, panel: 3 }));
        assert!(matches!(c.order_disclosure(req.clone(), None, &[(w(9), true)], 5, 0), Err(CourtError::Ruling(RulingError::StrangerVote(_)))));
        let mut co = req.clone();
        co.purpose = Purpose::CourtOrder;
        assert!(matches!(c.order_disclosure(co, None, &all_for(&panel), 5, 0), Err(CourtError::Unconstitutional { article: Article::NoDisclosureWithoutOrder, .. })));
        c.order_disclosure(req, None, &all_for(&panel), 5, 0).unwrap();
        assert_eq!(c.orders().len(), 1);
    }

    #[test]
    fn promotion_ladder_is_replayable_to_the_same_root() {
        let build = || {
            let mut c = seated();
            c.appoint(w(9), Rank::Clerk, 2).unwrap();
            assert!(!c.sit_exam(w(9), Course::ConstitutionalCode, 6_000, 3).unwrap());
            assert!(c.sit_exam(w(9), Course::ConstitutionalCode, 8_000, 4).unwrap());
            assert!(c.sit_exam(w(9), Course::Evidence, 9_000, 5).unwrap());
            let mut reg = UniversityRegistry::new();
            reg.mark_graduated(&w(9)).unwrap();
            c.admit_graduate(w(9), &reg, 6).unwrap();
            c.promote(w(9), Rank::Advocate, &[w(1), w(4)], 7).unwrap();
            c
        };
        let a = build();
        let b = build();
        assert_eq!(a.court_root(), b.court_root());
        assert_eq!(a.bench().rank_of(&w(9)), Some(Rank::Advocate));
        assert!(a.bench().get(&w(9)).unwrap().has(Credential::UniversityGraduate));
        assert_eq!(a.docket().by_wallet(&w(9)).len(), 9, "appoint + 3 exams + 3 credentials (2 courses + bar) + graduate credential + promotion");
    }

    /// The supreme honour is quorum-gated, and the gate is the nation's own rule — not the
    /// court's opinion of it. Even the operator cannot confer the Elephant alone.
    #[test]
    fn the_supreme_honour_cannot_be_conferred_alone_not_even_by_the_operator() {
        let mut c = seated();
        let rocky = w(1);
        // Operator alone: refused.
        let e = c.confer_honour(Order::Elefantordenen, "", rocky, "deeds", w(99), 0, true, 5).unwrap_err();
        assert!(matches!(e, CourtError::Unconstitutional { article: Article::PromotionByDeeds, .. }), "{e:?}");
        // Quorum without the operator: still refused.
        assert!(c.confer_honour(Order::Elefantordenen, "", rocky, "deeds", w(99), 3, false, 5).is_err());
        // A rank on the Elephant is malformed — the Elephant IS the rank.
        assert!(c.confer_honour(Order::Elefantordenen, "Ridder", rocky, "deeds", w(99), 3, true, 5).is_err());
        // Operator AND three approvals: conferred, on the docket, and on the chain event.
        let ev = c.confer_honour(Order::Elefantordenen, "", rocky, "kept the ledger honest", w(99), 3, true, 5).unwrap();
        assert!(matches!(ev, SigilEvent::HonorConferred { .. }));
        assert!(c.bench().get(&rocky).unwrap().has(Credential::Aeresborger));
        assert_eq!(c.docket().by_tag(19).len(), 1);
        // Soulbound: it cannot be conferred twice.
        assert!(c.confer_honour(Order::Elefantordenen, "", rocky, "again", w(99), 3, true, 6).is_err());
        // The working honour needs only one authority — a different rule, correctly applied.
        assert!(c.confer_honour(Order::Ridderkorset, "Ridder", w(2), "shipped the thing", w(99), 0, true, 7).is_ok());
        // And it cannot decorate someone with no seat.
        assert!(matches!(
            c.confer_honour(Order::Ridderkorset, "Ridder", w(200), "x", w(99), 0, true, 8),
            Err(CourtError::Bench(BenchError::NotMember))
        ));
    }

    #[test]
    fn supersede_never_deletes() {
        let mut c = seated();
        let n = c.docket().len();
        assert!(matches!(c.supersede(999, "typo", 3), Err(CourtError::Unconstitutional { article: Article::SupersedeNeverDelete, .. })));
        c.supersede(0, "wrong rank recorded", 3).unwrap();
        assert_eq!(c.docket().len(), n + 1);
        assert!(c.docket().get(0).is_some());
    }
}
