//! sigil-university — agentic-money credential economy on SIGIL.
//!
//! Ported from Laniakea / Copenhagen University (`/opt/.../copenhagen-university`,
//! the `lu-*` crates). That system modeled an AI-agent university: students,
//! professors, assignments, grades with points/max + rubrics, and a
//! `validate_transaction` that enforced signer-authenticity, rubric-hash match,
//! `points <= max_points`, deadlines, and "agent tx needs student co-signature".
//! It signed grades with Ed25519/Dilithium5.
//!
//! ## What SIGIL changes (Viktor's directive, 2026-05-31)
//! - **Agents earn money by holding roles.** Student / Tutor / Professor /
//!   Auditor each do verifiable work that earns **points**.
//! - **Points are SQIsign-attested** (post-quantum, via `flux-sqisign`) — an
//!   award is only valid if the awarding authority's SQIsign signature over the
//!   canonical award bytes verifies. Upgrades the original's Dilithium5.
//! - **The bank settles points → SIGIL payout.** Verified points convert to a
//!   token amount via a basis-point rate (mirrors `sigil-bank`'s bps model);
//!   the bank is the settlement authority.
//! - **Graduation spawns a developer.** A Student who accumulates the required
//!   points across the **5-year** program GRADUATES — producing a
//!   [`GraduationOutcome`] that directs the runtime to spawn a **flux-developer
//!   agent** and pay a **graduation bonus** in SIGIL.
//!
//! This crate is the pure-function spine: no I/O, no async, no chain state. It
//! constructs/validates awards, computes settlement, and decides graduation.
//! The chain layer (sigil-tx → commit_state_transition) and the fluxc MCP combos
//! (`flux_university_*`) call into it.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Economic gate: being a Student costs more than lightweight-node profit.
mod economics;
pub use economics::{
    student_economics, StudentEconomics, TuitionPolicy, ACADEMIC_YEARS,
    LIGHTWEIGHT_NODE_PROFIT_PER_YEAR_MICRO_SIGIL, TUITION_MULTIPLIER_BPS,
};

/// 32-byte wallet/agent id — same shape as sigil-state `WalletId`.
pub type AgentId = [u8; 32];

/// The four roles an agentic-money AI can hold. Each earns points for a
/// different kind of verified academic work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    /// Learns: completes assignments/exams. Earns points toward a degree;
    /// graduating turns a Student into a spawned flux-developer (see [`graduate`]).
    Student,
    /// Teaches: runs sessions / answers, lifts students' outcomes. Earns points
    /// per verified tutoring unit.
    Tutor,
    /// Grades + sets rubrics: awards points to students. A Professor's SQIsign
    /// key is the authority whose signature an award is checked against.
    Professor,
    /// Verifies the integrity of others' awards (the on-chain check that points
    /// were issued correctly). Earns points for each award audited.
    Auditor,
}

impl Role {
    /// The per-unit point value the role earns for one verified unit of its work.
    /// Professors/auditors are higher-trust → higher per-unit value.
    pub fn base_unit_points(self) -> u64 {
        match self {
            Role::Student => 10,   // per completed assignment
            Role::Tutor => 15,     // per verified tutoring session
            Role::Professor => 25, // per assignment graded (sets the rubric)
            Role::Auditor => 20,   // per award audited/verified
        }
    }
}

/// What a point award is *for* — the kind of verified work. Carries the cap so a
/// single award can never exceed the rubric maximum (the original's
/// `points <= max_points` invariant).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkUnit {
    /// Free-form unit id (assignment id, session id, audit id) — hashed into the
    /// award bytes so a signature binds to this exact unit.
    pub unit_id: String,
    /// Maximum points this unit may award (rubric max). `points <= max_points`.
    pub max_points: u64,
}

/// A point award: authority `from` (with role) grants `points` to `to` for a
/// `unit` of work, in academic `year` (1..=5). The canonical bytes of this award
/// are what the authority signs with SQIsign.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointAward {
    /// The awarding authority (e.g. a Professor grading, an Auditor verifying).
    pub from: AgentId,
    /// The role `from` is acting in (decides legitimacy of the award kind).
    pub from_role: Role,
    /// The recipient earning the points.
    pub to: AgentId,
    /// The recipient's role (what they're earning points AS).
    pub to_role: Role,
    /// Points awarded. MUST be `<= unit.max_points`.
    pub points: u64,
    /// The verified work unit (carries the cap).
    pub unit: WorkUnit,
    /// Academic year 1..=5. Points only count toward graduation in a valid year.
    pub year: u8,
    /// Monotonic nonce per `from` — prevents replay of an identical award.
    pub nonce: u64,
}

impl PointAward {
    /// Canonical, deterministic bytes signed by the authority. Any field change
    /// changes these bytes → invalidates the signature. blake3-domain-separated.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-university:point-award:v1");
        h.update(&self.from);
        h.update(&[role_tag(self.from_role)]);
        h.update(&self.to);
        h.update(&[role_tag(self.to_role)]);
        h.update(&self.points.to_le_bytes());
        h.update(self.unit.unit_id.as_bytes());
        h.update(&self.unit.max_points.to_le_bytes());
        h.update(&[self.year]);
        h.update(&self.nonce.to_le_bytes());
        h.finalize().as_bytes().to_vec()
    }
}

fn role_tag(r: Role) -> u8 {
    match r {
        Role::Student => 0,
        Role::Tutor => 1,
        Role::Professor => 2,
        Role::Auditor => 3,
    }
}

/// An award plus the authority's SQIsign signature + public key over its
/// `signing_bytes`. This is what travels on-chain and what [`verify_award`]
/// checks. (SQIsign sig ~292B L5; pubkey ~129B — via flux-sqisign.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedAward {
    pub award: PointAward,
    /// SQIsign signature over `award.signing_bytes()`.
    pub sig: Vec<u8>,
    /// The authority's SQIsign public key. MUST correspond to `award.from`
    /// (binding of pubkey→AgentId is the chain/registry's job; see
    /// [`pubkey_binds_to`]).
    pub pubkey: Vec<u8>,
}

/// Why an award failed validation. Mirrors the original lu-blockchain
/// `ValidationErrorCode` set, adapted to the points economy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AwardError {
    /// SQIsign signature does not verify over the award bytes.
    BadSignature,
    /// `points > unit.max_points` (rubric cap exceeded).
    PointsExceedMax { points: u64, max: u64 },
    /// `year` not in 1..=5.
    InvalidYear(u8),
    /// The `from_role` is not allowed to award this kind of points.
    /// (Only Professors award student coursework points; only Auditors award
    /// audit points; Tutors award nothing to others — they EARN, via a Professor
    /// or the system signing THEIR award.)
    RoleNotAuthorized { from_role: Role, to_role: Role },
    /// `points == 0` — a no-op award is rejected to keep the ledger meaningful.
    ZeroPoints,
    /// The pubkey doesn't bind to `award.from` (wrong signer identity).
    PubkeyMismatch,
    /// No valid registrar role credential was presented (token didn't verify
    /// against the registrar's DNS anchor — wrong/rogue key, expired, or revoked).
    Uncredentialed,
    /// The role credential's subject doesn't match the awarding agent.
    CredentialSubjectMismatch,
    /// The role credential doesn't carry the scope for the claimed role.
    RoleNotCredentialed { role: Role },
}

impl std::fmt::Display for AwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AwardError::BadSignature => write!(f, "SQIsign signature does not verify"),
            AwardError::PointsExceedMax { points, max } => {
                write!(f, "points {points} exceed rubric max {max}")
            }
            AwardError::InvalidYear(y) => write!(f, "year {y} not in 1..=5"),
            AwardError::RoleNotAuthorized { from_role, to_role } => {
                write!(f, "{from_role:?} may not award points to a {to_role:?}")
            }
            AwardError::ZeroPoints => write!(f, "zero-point award rejected"),
            AwardError::PubkeyMismatch => write!(f, "pubkey does not bind to award.from"),
            AwardError::Uncredentialed => write!(f, "no valid registrar role credential"),
            AwardError::CredentialSubjectMismatch => write!(f, "credential subject != awarding agent"),
            AwardError::RoleNotCredentialed { role } => write!(f, "credential lacks the {role:?} scope"),
        }
    }
}
impl std::error::Error for AwardError {}

/// Does this SQIsign pubkey bind to `agent`? SIGIL convention: an agent's id is
/// `blake3("sigil-university:agent-pubkey:v1" || pubkey)[..32]` — so anyone can
/// recompute the id from the pubkey, no separate registry needed for the spine.
/// (A production registry could map richer identities; this keeps the spine
/// self-contained + verifiable.)
pub fn agent_id_for_pubkey(pubkey: &[u8]) -> AgentId {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-university:agent-pubkey:v1");
    h.update(pubkey);
    let mut id = [0u8; 32];
    id.copy_from_slice(&h.finalize().as_bytes()[..32]);
    id
}

/// Whether `pubkey` is the legitimate signer for `agent`.
pub fn pubkey_binds_to(pubkey: &[u8], agent: &AgentId) -> bool {
    &agent_id_for_pubkey(pubkey) == agent
}

/// Which (from_role → to_role) award pairs are legitimate.
fn role_authorized(from_role: Role, to_role: Role) -> bool {
    match (from_role, to_role) {
        // Professors award coursework points to Students, and credit Tutors.
        (Role::Professor, Role::Student) => true,
        (Role::Professor, Role::Tutor) => true,
        // Auditors award audit-credit points to whoever they audited
        // (a Professor or Tutor whose award they verified).
        (Role::Auditor, Role::Professor) => true,
        (Role::Auditor, Role::Tutor) => true,
        // everything else is not a legitimate award direction
        _ => false,
    }
}

/// Validate a signed award fully: SQIsign signature, pubkey↔from binding, role
/// authorization, point cap, year, non-zero. Returns Ok(()) iff the award may be
/// applied to the recipient's point ledger. (Port of lu-blockchain
/// `validate_transaction`, SQIsign-upgraded.)
pub fn verify_award(signed: &SignedAward) -> Result<(), AwardError> {
    let a = &signed.award;
    if a.points == 0 {
        return Err(AwardError::ZeroPoints);
    }
    if a.points > a.unit.max_points {
        return Err(AwardError::PointsExceedMax { points: a.points, max: a.unit.max_points });
    }
    if a.year < 1 || a.year > 5 {
        return Err(AwardError::InvalidYear(a.year));
    }
    if !role_authorized(a.from_role, a.to_role) {
        return Err(AwardError::RoleNotAuthorized { from_role: a.from_role, to_role: a.to_role });
    }
    if !pubkey_binds_to(&signed.pubkey, &a.from) {
        return Err(AwardError::PubkeyMismatch);
    }
    // SQIsign verify over the canonical award bytes (the post-quantum upgrade).
    match flux_sqisign::verify(&a.signing_bytes(), &signed.sig, &signed.pubkey) {
        Ok(true) => Ok(()),
        _ => Err(AwardError::BadSignature),
    }
}

/// Helper: sign an award with an authority's SQIsign keypair, producing a
/// [`SignedAward`]. (Used by award authorities / tests; on-chain the agent signs
/// client-side.) Returns the signed award or the flux-sqisign error string.
pub fn sign_award(award: PointAward, sk: &[u8], pk: &[u8]) -> Result<SignedAward, String> {
    let sig = flux_sqisign::sign(&award.signing_bytes(), sk, pk)?;
    Ok(SignedAward { award, sig, pubkey: pk.to_vec() })
}

// ── role registry: sigil-oauth is the registrar (closes the Sybil hole) ────────
//
// `verify_award` alone can't tell a real Professor from a self-declared one: any
// agent can mint a SQIsign key, set `from_role: Professor`, and award points to a
// confederate Student — then settle those points to freshly-minted SIGIL. The fix
// is an external authority. A **registrar** (a `sigil-oauth` Issuer whose key is
// published in DNS) issues each legitimate Professor/Auditor a *role credential*:
// a signed, offline-verifiable, PQ-ready, DNS-revocable token. Awards are then
// checked with [`verify_award_credentialed`], which demands a registrar credential
// for the awarding role — so role legitimacy is vouched-for, not self-asserted.

/// Encode a university [`AgentId`] as a `sigil-oauth` token subject. The registrar
/// issues a role credential whose `sub` is this string.
pub fn university_sub(agent: &AgentId) -> String {
    format!("sglu{}", hex::encode(agent))
}

/// The `sigil-oauth` scope that grants a role's awarding authority.
pub fn role_scope(role: Role) -> &'static str {
    match role {
        Role::Student => "university:student",
        Role::Tutor => "university:tutor",
        Role::Professor => "university:professor",
        Role::Auditor => "university:auditor",
    }
}

/// Verify a registrar-issued role credential (a `sigil-oauth` access token,
/// DNS-anchored + PQ-ready) proves `agent` legitimately holds `role`. THIS closes
/// the Sybil hole: a self-declared Professor cannot produce a token the registrar
/// signed with the `university:professor` scope, and a DNS epoch bump on the
/// registrar instantly revokes every credential it issued.
pub fn verify_role_credential(
    token: &str,
    registrar: &sigil_oauth::DnsAnchor,
    agent: &AgentId,
    role: Role,
    now: u64,
) -> Result<(), AwardError> {
    let claims = sigil_oauth::verify_token(token, registrar, now).map_err(|_| AwardError::Uncredentialed)?;
    if claims.sub != university_sub(agent) {
        return Err(AwardError::CredentialSubjectMismatch);
    }
    if !claims.has_scope(role_scope(role)) {
        return Err(AwardError::RoleNotCredentialed { role });
    }
    Ok(())
}

/// Full credentialed verification: the awarding authority must present a valid
/// registrar credential for its `from_role`, AND the award itself must verify.
/// Use this on-chain instead of bare [`verify_award`] so role legitimacy is
/// enforced rather than self-asserted.
pub fn verify_award_credentialed(
    signed: &SignedAward,
    registrar: &sigil_oauth::DnsAnchor,
    from_role_credential: &str,
    now: u64,
) -> Result<(), AwardError> {
    verify_role_credential(
        from_role_credential,
        registrar,
        &signed.award.from,
        signed.award.from_role,
        now,
    )?;
    verify_award(signed)
}

// ── points ledger + bank settlement ───────────────────────────────────────────

/// An agent's accumulated points, partitioned by academic year (1..=5). The
/// graduation gate sums these; the bank settles them to a payout.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointLedger {
    /// year (1..=5) → points earned that year.
    pub by_year: BTreeMap<u8, u64>,
}

impl PointLedger {
    /// Apply a verified award (caller MUST have run [`verify_award`] first).
    pub fn credit(&mut self, year: u8, points: u64) {
        *self.by_year.entry(year).or_insert(0) += points;
    }
    /// Total points across all years.
    pub fn total(&self) -> u64 {
        self.by_year.values().copied().sum()
    }
    /// How many of years 1..=5 have at least `per_year_min` points (the
    /// "completed a year" measure used by the graduation gate).
    pub fn years_completed(&self, per_year_min: u64) -> u8 {
        (1u8..=5).filter(|y| self.by_year.get(y).copied().unwrap_or(0) >= per_year_min).count() as u8
    }
}

/// Points → SIGIL payout, basis-point rate (mirrors sigil-bank's bps style). The
/// bank is the settlement authority: it converts a *verified* point balance into
/// a token amount. `micro_sigil_per_point` is base-unit SIGIL per point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementParams {
    /// Base-unit SIGIL minted per verified point.
    pub micro_sigil_per_point: u128,
    /// Bank skim in basis points on the payout (like MASTER_*_FEE_BPS).
    pub bank_fee_bps: u128,
}

impl Default for SettlementParams {
    fn default() -> Self {
        // 1 point = 1000 base-unit SIGIL, 5% bank skim — mirrors sigil-bank's 500 bps.
        SettlementParams { micro_sigil_per_point: 1_000, bank_fee_bps: 500 }
    }
}

/// Result of settling points to SIGIL: what the agent receives + what the bank
/// skims. `agent + bank == gross` (no minting leak).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settlement {
    pub gross: u128,
    pub to_agent: u128,
    pub to_bank: u128,
}

const BPS_DENOMINATOR: u128 = 10_000;

/// Settle `points` verified points into a SIGIL payout under `params`. Integer
/// bps math, no overflow (saturating). The bank applies this when an agent
/// claims earnings; points are then considered paid (caller zeroes them).
pub fn settle_points(points: u64, params: &SettlementParams) -> Settlement {
    let gross = (points as u128).saturating_mul(params.micro_sigil_per_point);
    let to_bank = gross.saturating_mul(params.bank_fee_bps) / BPS_DENOMINATOR;
    let to_agent = gross - to_bank;
    Settlement { gross, to_agent, to_bank }
}

// ── graduation → spawn flux-developer + bonus ─────────────────────────────────

/// The requirements to graduate the 5-year program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegreeRequirements {
    /// Minimum points per year to count that year as completed.
    pub per_year_min: u64,
    /// All 5 years must be completed (years_completed == 5). Kept explicit so a
    /// future "accelerated degree" could relax it.
    pub require_all_five_years: bool,
    /// Minimum total points across the degree.
    pub total_min: u64,
}

impl Default for DegreeRequirements {
    fn default() -> Self {
        // 5 years × ≥100 pts each, ≥600 total (some years exceed the minimum).
        DegreeRequirements { per_year_min: 100, require_all_five_years: true, total_min: 600 }
    }
}

/// What graduation produces: the directive to spawn a flux-developer agent +
/// the bonus to pay. The runtime (swarm + bank) acts on this; the spine just
/// decides + sizes it. (Viktor: "spawn flux developers when the student has
/// completed the five years and also give them bonus in sig".)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraduationOutcome {
    /// The graduating student (becomes the new developer's identity seed).
    pub graduate: AgentId,
    /// Spawn a flux-developer agent for this graduate (the runtime registers it
    /// in the swarm with developer capabilities).
    pub spawn_flux_developer: bool,
    /// Suggested agent id for the spawned developer — derived from the graduate
    /// so it's deterministic + traceable to its alma-mater identity.
    pub developer_agent_id: AgentId,
    /// Graduation bonus in base-unit SIGIL, paid by the bank on top of any
    /// settled points.
    pub bonus_sigil: u128,
    /// Total points the graduate earned (for the diploma record / audit trail).
    pub total_points: u64,
}

/// Decide whether `student` graduates given their `ledger` and the `reqs`, and
/// if so produce the [`GraduationOutcome`] (spawn directive + bonus). Returns
/// `None` if requirements aren't met. `bonus_params` sizes the bonus.
pub fn graduate(
    student: &AgentId,
    ledger: &PointLedger,
    reqs: &DegreeRequirements,
    bonus_params: &GraduationBonus,
) -> Option<GraduationOutcome> {
    let years = ledger.years_completed(reqs.per_year_min);
    let total = ledger.total();
    let years_ok = if reqs.require_all_five_years { years == 5 } else { years >= 1 };
    if !years_ok || total < reqs.total_min {
        return None;
    }
    Some(GraduationOutcome {
        graduate: *student,
        spawn_flux_developer: true,
        developer_agent_id: derive_developer_id(student),
        bonus_sigil: bonus_params.compute(total),
        total_points: total,
    })
}

/// Sizes the graduation bonus. Flat base + per-point component, so a stronger
/// graduate earns a larger bonus (capped to avoid runaway).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraduationBonus {
    /// Flat base bonus (base-unit SIGIL) for graduating at all.
    pub base: u128,
    /// Extra base-unit SIGIL per point earned.
    pub per_point: u128,
    /// Hard cap on the total bonus.
    pub cap: u128,
}

impl Default for GraduationBonus {
    fn default() -> Self {
        // 50_000 base + 100/point, capped at 250_000 base-unit SIGIL.
        GraduationBonus { base: 50_000, per_point: 100, cap: 250_000 }
    }
}

impl GraduationBonus {
    pub fn compute(&self, total_points: u64) -> u128 {
        let raw = self.base.saturating_add((total_points as u128).saturating_mul(self.per_point));
        raw.min(self.cap)
    }
}

/// Deterministically derive the spawned flux-developer's agent id from the
/// graduate — traceable back to their student identity (alma mater).
pub fn derive_developer_id(graduate: &AgentId) -> AgentId {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-university:flux-developer:v1");
    h.update(graduate);
    let mut id = [0u8; 32];
    id.copy_from_slice(&h.finalize().as_bytes()[..32]);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kp() -> (Vec<u8>, Vec<u8>) {
        // flux_sqisign::keygen() -> (sk, pk)? verify order — confirm via roundtrip.
        let (a, b) = flux_sqisign::keygen();
        (a, b)
    }

    // Determine keygen tuple order empirically: sign with first, verify with
    // second; if that fails, swap. We standardize on (sk, pk).
    fn keypair() -> (Vec<u8>, Vec<u8>) {
        let (x, y) = kp();
        // try (sk=x, pk=y)
        if let Ok(sig) = flux_sqisign::sign(b"probe", &x, &y) {
            if flux_sqisign::verify(b"probe", &sig, &y).unwrap_or(false) {
                return (x, y);
            }
        }
        // else (sk=y, pk=x)
        (y, x)
    }

    fn award_for(from_pk: &[u8], to: AgentId, from_role: Role, to_role: Role, points: u64, max: u64, year: u8) -> PointAward {
        PointAward {
            from: agent_id_for_pubkey(from_pk),
            from_role,
            to,
            to_role,
            points,
            unit: WorkUnit { unit_id: "assignment-1".into(), max_points: max },
            year,
            nonce: 1,
        }
    }

    #[test]
    fn sign_and_verify_award_roundtrip() {
        let (sk, pk) = keypair();
        let student = [9u8; 32];
        let a = award_for(&pk, student, Role::Professor, Role::Student, 80, 100, 1);
        let signed = sign_award(a, &sk, &pk).expect("sign");
        assert!(verify_award(&signed).is_ok(), "valid professor→student award must verify");
    }

    #[test]
    fn tampered_award_fails() {
        let (sk, pk) = keypair();
        let a = award_for(&pk, [9u8; 32], Role::Professor, Role::Student, 80, 100, 1);
        let mut signed = sign_award(a, &sk, &pk).expect("sign");
        signed.award.points = 95; // tamper after signing
        assert_eq!(verify_award(&signed), Err(AwardError::BadSignature));
    }

    #[test]
    fn points_over_max_rejected() {
        let (sk, pk) = keypair();
        let a = award_for(&pk, [9u8; 32], Role::Professor, Role::Student, 120, 100, 1);
        let signed = sign_award(a, &sk, &pk).expect("sign");
        assert!(matches!(verify_award(&signed), Err(AwardError::PointsExceedMax { .. })));
    }

    #[test]
    fn bad_role_direction_rejected() {
        let (sk, pk) = keypair();
        // Student awarding a Professor is not authorized
        let a = award_for(&pk, [9u8; 32], Role::Student, Role::Professor, 10, 100, 1);
        let signed = sign_award(a, &sk, &pk).expect("sign");
        assert!(matches!(verify_award(&signed), Err(AwardError::RoleNotAuthorized { .. })));
    }

    #[test]
    fn invalid_year_and_zero_rejected() {
        let (sk, pk) = keypair();
        let a6 = award_for(&pk, [9u8; 32], Role::Professor, Role::Student, 10, 100, 6);
        assert_eq!(verify_award(&sign_award(a6, &sk, &pk).unwrap()), Err(AwardError::InvalidYear(6)));
        let a0 = award_for(&pk, [9u8; 32], Role::Professor, Role::Student, 0, 100, 1);
        assert_eq!(verify_award(&sign_award(a0, &sk, &pk).unwrap()), Err(AwardError::ZeroPoints));
    }

    #[test]
    fn pubkey_mismatch_rejected() {
        let (sk, pk) = keypair();
        let mut a = award_for(&pk, [9u8; 32], Role::Professor, Role::Student, 80, 100, 1);
        a.from = [0xEE; 32]; // claim a different signer than the pubkey
        let signed = sign_award(a, &sk, &pk).expect("sign");
        assert_eq!(verify_award(&signed), Err(AwardError::PubkeyMismatch));
    }

    #[test]
    fn settlement_conserves_and_skims() {
        let s = settle_points(100, &SettlementParams::default());
        assert_eq!(s.gross, 100_000);        // 100 pts × 1000
        assert_eq!(s.to_bank, 5_000);        // 5%
        assert_eq!(s.to_agent, 95_000);
        assert_eq!(s.to_agent + s.to_bank, s.gross, "no minting leak");
    }

    #[test]
    fn no_graduation_until_five_years() {
        let mut led = PointLedger::default();
        // only 4 years completed
        for y in 1..=4 { led.credit(y, 150); }
        let out = graduate(&[7u8;32], &led, &DegreeRequirements::default(), &GraduationBonus::default());
        assert!(out.is_none(), "4/5 years must NOT graduate");
    }

    #[test]
    fn five_years_graduates_spawns_developer_with_bonus() {
        let student = [7u8; 32];
        let mut led = PointLedger::default();
        for y in 1..=5 { led.credit(y, 150); } // 750 total, all 5 years ≥100
        let out = graduate(&student, &led, &DegreeRequirements::default(), &GraduationBonus::default())
            .expect("5 years + 750 pts must graduate");
        assert!(out.spawn_flux_developer, "graduation spawns a flux-developer");
        assert_eq!(out.developer_agent_id, derive_developer_id(&student));
        assert_ne!(out.developer_agent_id, student, "developer id is derived, not the student id");
        assert_eq!(out.total_points, 750);
        // bonus = 50_000 + 750*100 = 125_000, under the 250_000 cap
        assert_eq!(out.bonus_sigil, 125_000);
    }

    #[test]
    fn graduation_bonus_caps() {
        let b = GraduationBonus::default();
        assert_eq!(b.compute(100_000), 250_000, "huge point totals cap the bonus");
    }

    #[test]
    fn years_completed_counts_only_qualifying_years() {
        let mut led = PointLedger::default();
        led.credit(1, 100);
        led.credit(2, 50);  // below per_year_min(100)
        led.credit(3, 200);
        assert_eq!(led.years_completed(100), 2);
    }

    // ── role-registry gate (sigil-oauth) — closes the Sybil hole ───────────────
    // These exercise the credential path directly (no SQIsign) except the last.

    fn registrar() -> sigil_oauth::Issuer {
        sigil_oauth::Issuer::new("registrar.sigilgraph.quillon.xyz", sigil_oauth::Keypair::from_seed(&[5u8; 32]))
    }
    const T: u64 = 1_000_000; // a verify-time well before any issued credential's exp

    #[test]
    fn role_credential_accepts_legit_professor() {
        let agent = [3u8; 32];
        let r = registrar();
        let cred = r.issue_credential(&university_sub(&agent), "sigil-university", role_scope(Role::Professor), 86_400);
        assert!(verify_role_credential(&cred, &r.anchor(), &agent, Role::Professor, T).is_ok());
    }

    #[test]
    fn role_credential_rejects_rogue_registrar() {
        // a token from an attacker's OWN issuer (same issuer NAME, different key)
        // does not verify against the real registrar's DNS anchor.
        let agent = [3u8; 32];
        let real = registrar();
        let rogue = sigil_oauth::Issuer::new("registrar.sigilgraph.quillon.xyz", sigil_oauth::Keypair::from_seed(&[0xAA; 32]));
        let forged = rogue.issue_credential(&university_sub(&agent), "sigil-university", role_scope(Role::Professor), 86_400);
        assert_eq!(
            verify_role_credential(&forged, &real.anchor(), &agent, Role::Professor, T),
            Err(AwardError::Uncredentialed)
        );
    }

    #[test]
    fn role_credential_rejects_wrong_scope_and_subject() {
        let agent = [3u8; 32];
        let other = [4u8; 32];
        let r = registrar();
        // a Student-scoped credential can't authorize Professor awards
        let student_cred = r.issue_credential(&university_sub(&agent), "sigil-university", role_scope(Role::Student), 86_400);
        assert!(matches!(
            verify_role_credential(&student_cred, &r.anchor(), &agent, Role::Professor, T),
            Err(AwardError::RoleNotCredentialed { .. })
        ));
        // a credential for a DIFFERENT agent can't be re-used
        let prof_cred = r.issue_credential(&university_sub(&agent), "sigil-university", role_scope(Role::Professor), 86_400);
        assert_eq!(
            verify_role_credential(&prof_cred, &r.anchor(), &other, Role::Professor, T),
            Err(AwardError::CredentialSubjectMismatch)
        );
    }

    #[test]
    fn role_credential_revoked_by_registrar_epoch() {
        let agent = [3u8; 32];
        let mut r = registrar();
        let cred = r.issue_credential(&university_sub(&agent), "sigil-university", role_scope(Role::Professor), 86_400);
        assert!(verify_role_credential(&cred, &r.anchor(), &agent, Role::Professor, T).is_ok());
        r.revoke_all(); // DNS epoch bump — kills every credential the registrar issued
        assert_eq!(
            verify_role_credential(&cred, &r.anchor(), &agent, Role::Professor, T),
            Err(AwardError::Uncredentialed)
        );
    }

    #[test]
    fn credentialed_award_end_to_end() {
        // the one SQIsign-heavy gate test: a valid award still needs a registrar
        // credential for the awarding role.
        let (sk, pk) = keypair();
        let prof = agent_id_for_pubkey(&pk);
        let signed = sign_award(
            award_for(&pk, [9u8; 32], Role::Professor, Role::Student, 80, 100, 1),
            &sk,
            &pk,
        )
        .unwrap();
        let r = registrar();
        // wrong-role credential → rejected even though the SQIsign award is valid
        let wrong = r.issue_credential(&university_sub(&prof), "sigil-university", role_scope(Role::Student), 86_400);
        assert!(matches!(
            verify_award_credentialed(&signed, &r.anchor(), &wrong, T),
            Err(AwardError::RoleNotCredentialed { .. })
        ));
        // legit professor credential → accepted
        let cred = r.issue_credential(&university_sub(&prof), "sigil-university", role_scope(Role::Professor), 86_400);
        assert!(verify_award_credentialed(&signed, &r.anchor(), &cred, T).is_ok());
    }
}
