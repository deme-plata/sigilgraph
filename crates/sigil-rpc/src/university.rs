//! University money-bridge: wires the pure `sigil-university` spine to the SIGIL
//! state chokepoint.
//!
//! The crate `sigil-university` is intentionally I/O-free — it constructs and
//! verifies awards, scores graduation, and sizes settlements, but never touches
//! a balance. This module is the bridge: it takes those decisions and turns them
//! into `commit_state_transition` mutations, exactly like `credit_share` /
//! `execute_swap` do for mining / DEX.
//!
//! ## Money model — TREASURY, NOT MINT (the safety property)
//! Every SIGIL move here is a **transfer to/from a fixed university treasury**:
//!   * `pay_tuition`  : student  → treasury   (funds the economy)
//!   * `settle`       : treasury → agent (+ bank fee → master)
//!   * `graduate`     : treasury → graduate   (the bonus)
//! Nothing is minted. Total NATIVE supply is invariant across all three, so the
//! 21M cap is untouched and the "anon mint" class of bug is impossible by
//! construction — the treasury can only pay out what tuition + funding put in
//! (an underfunded treasury returns [`UniversityError::TreasuryUnderfunded`],
//! it never goes negative). The academic bookkeeping (who earned what) lives in
//! [`sigil_university::UniversityRegistry`], persisted by the node under its own
//! flux-db key — this module only moves money against it.

use sigil_state::{
    commit_state_transition, SigilState, StateMutation, StateTransition, WalletId, NATIVE,
};
use sigil_university::{
    graduate as score_graduation, settle_points, verify_award_credentialed, AwardError,
    DegreeRequirements, GraduationBonus, GraduationOutcome, RegistryError, Settlement,
    SettlementParams, SignedAward, TuitionPolicy, UniversityRegistry,
};

/// The university treasury — a fixed protocol address (sibling to
/// `crate::COMMONS_WALLET = [0xC0; 32]`). Holds tuition inflows and pays
/// settlements + graduation bonuses. Like the master/commons wallets, it's a
/// well-known constant, not a user wallet.
pub const UNIVERSITY_TREASURY: WalletId = [0xC2; 32];

/// Why a university money operation failed.
#[derive(Debug, thiserror::Error)]
pub enum UniversityError {
    /// Student's NATIVE balance can't cover the year's tuition.
    #[error("student has insufficient NATIVE balance for tuition")]
    StudentUnderfunded,
    /// The treasury can't cover this settlement / bonus payout.
    #[error("university treasury underfunded for this payout")]
    TreasuryUnderfunded,
    /// The signed award failed credentialed verification (sig / role / cap / year).
    #[error("award rejected: {0}")]
    Award(AwardError),
    /// Registry bookkeeping rejected the operation (replay / tuition / graduated).
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
    /// The state chokepoint rejected the transition.
    #[error("commit: {0}")]
    Commit(#[from] sigil_state::CommitError),
    /// u128 arithmetic overflow in a balance update.
    #[error("arithmetic overflow")]
    Overflow,
}

/// Pay one academic year's tuition: transfer `policy.tuition_per_year()` NATIVE
/// from `student` to the [`UNIVERSITY_TREASURY`], and mark the year paid in the
/// registry (which gates the student's coursework-point earning for that year).
///
/// Conserves supply (pure transfer). Returns the tuition amount moved.
pub fn pay_tuition(
    state: &mut SigilState,
    height: u64,
    reg: &mut UniversityRegistry,
    student: WalletId,
    policy: &TuitionPolicy,
    year: u8,
) -> Result<u128, UniversityError> {
    let tuition = policy.tuition_per_year();
    let student_pre = state.balance_of(&student, &NATIVE);
    if student_pre < tuition {
        return Err(UniversityError::StudentUnderfunded);
    }
    let treasury_pre = state.balance_of(&UNIVERSITY_TREASURY, &NATIVE);
    let mutations = vec![
        StateMutation::SetBalance {
            wallet: student,
            token: NATIVE,
            amount: student_pre - tuition,
        },
        StateMutation::SetBalance {
            wallet: UNIVERSITY_TREASURY,
            token: NATIVE,
            amount: treasury_pre.checked_add(tuition).ok_or(UniversityError::Overflow)?,
        },
    ];
    commit_state_transition(state, &StateTransition { at_height: height, mutations }, height)?;
    // Mark paid ONLY after the money moved (a failed commit leaves no false mark).
    reg.mark_tuition_paid(&student, year);
    Ok(tuition)
}

/// Verify a credentialed signed award and, if valid, record it into the
/// recipient's transcript. No money moves — points are academic credit, settled
/// to SIGIL later via [`settle`]. Returns the recipient's new lifetime total.
///
/// `registrar` is the `sigil-oauth` DNS anchor that vouches for the awarding
/// authority's role (closes the self-declared-Professor Sybil hole);
/// `from_role_credential` is the registrar token the authority presents.
pub fn record_award(
    reg: &mut UniversityRegistry,
    signed: &SignedAward,
    registrar: &sigil_oauth::DnsAnchor,
    from_role_credential: &str,
    now: u64,
) -> Result<u64, UniversityError> {
    verify_award_credentialed(signed, registrar, from_role_credential, now)
        .map_err(UniversityError::Award)?;
    Ok(reg.record_award(signed)?)
}

/// Settle an agent's unsettled points into a SIGIL payout from the treasury.
///
/// Pays `to_agent` to the agent and the `to_bank` skim to the master wallet (if
/// one is set and distinct from the agent; otherwise the treasury keeps the
/// skim). Debits the treasury by exactly what it pays out — supply-conserving.
/// Advances the registry's settled high-water-mark ONLY after the money lands,
/// so a treasury shortfall leaves the points still claimable.
pub fn settle(
    state: &mut SigilState,
    height: u64,
    reg: &mut UniversityRegistry,
    agent: WalletId,
    params: &SettlementParams,
) -> Result<Settlement, UniversityError> {
    let points = reg.unsettled_points(&agent);
    if points == 0 {
        return Ok(Settlement { gross: 0, to_agent: 0, to_bank: 0 });
    }
    let s = settle_points(points, params);

    let treasury_pre = state.balance_of(&UNIVERSITY_TREASURY, &NATIVE);
    let agent_pre = state.balance_of(&agent, &NATIVE);
    let master = state.master_wallet();

    let mut mutations = Vec::with_capacity(3);
    let agent_credit = s.to_agent;
    match master {
        // Distinct master with a non-zero skim: treasury pays the full gross,
        // split agent + master.
        Some(m) if m != agent && m != UNIVERSITY_TREASURY && s.to_bank > 0 => {
            if treasury_pre < s.gross {
                return Err(UniversityError::TreasuryUnderfunded);
            }
            let master_pre = state.balance_of(&m, &NATIVE);
            mutations.push(StateMutation::SetBalance {
                wallet: UNIVERSITY_TREASURY,
                token: NATIVE,
                amount: treasury_pre - s.gross,
            });
            mutations.push(StateMutation::SetBalance {
                wallet: agent,
                token: NATIVE,
                amount: agent_pre.checked_add(agent_credit).ok_or(UniversityError::Overflow)?,
            });
            mutations.push(StateMutation::SetBalance {
                wallet: m,
                token: NATIVE,
                amount: master_pre.checked_add(s.to_bank).ok_or(UniversityError::Overflow)?,
            });
        }
        // No distinct master / zero skim: treasury keeps the bank fee, pays only
        // the agent's share.
        _ => {
            if treasury_pre < agent_credit {
                return Err(UniversityError::TreasuryUnderfunded);
            }
            mutations.push(StateMutation::SetBalance {
                wallet: UNIVERSITY_TREASURY,
                token: NATIVE,
                amount: treasury_pre - agent_credit,
            });
            mutations.push(StateMutation::SetBalance {
                wallet: agent,
                token: NATIVE,
                amount: agent_pre.checked_add(agent_credit).ok_or(UniversityError::Overflow)?,
            });
        }
    }

    commit_state_transition(state, &StateTransition { at_height: height, mutations }, height)?;
    // Advance the high-water-mark now that the payout committed.
    reg.claim_settlement(&agent);
    Ok(s)
}

/// Attempt to graduate `student`. If they meet `reqs` (and haven't already
/// graduated), pay the graduation `bonus` from the treasury to the student and
/// return the [`GraduationOutcome`] (which carries the spawn-flux-developer
/// directive for the runtime to act on). Returns `Ok(None)` if requirements
/// aren't met or the student already graduated.
pub fn graduate(
    state: &mut SigilState,
    height: u64,
    reg: &mut UniversityRegistry,
    student: WalletId,
    reqs: &DegreeRequirements,
    bonus: &GraduationBonus,
) -> Result<Option<GraduationOutcome>, UniversityError> {
    if reg.is_graduated(&student) {
        return Ok(None);
    }
    let ledger = reg.ledger_of(&student);
    let outcome = match score_graduation(&student, &ledger, reqs, bonus) {
        Some(o) => o,
        None => return Ok(None),
    };

    // Pay the bonus (treasury → graduate) before recording the diploma, so a
    // treasury shortfall doesn't burn the one-shot graduation marker.
    let bonus_amt = outcome.bonus_sigil;
    if bonus_amt > 0 {
        let treasury_pre = state.balance_of(&UNIVERSITY_TREASURY, &NATIVE);
        if treasury_pre < bonus_amt {
            return Err(UniversityError::TreasuryUnderfunded);
        }
        let student_pre = state.balance_of(&student, &NATIVE);
        let mutations = vec![
            StateMutation::SetBalance {
                wallet: UNIVERSITY_TREASURY,
                token: NATIVE,
                amount: treasury_pre - bonus_amt,
            },
            StateMutation::SetBalance {
                wallet: student,
                token: NATIVE,
                amount: student_pre.checked_add(bonus_amt).ok_or(UniversityError::Overflow)?,
            },
        ];
        commit_state_transition(state, &StateTransition { at_height: height, mutations }, height)?;
    }
    reg.mark_graduated(&student)?;
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_university::{PointAward, Role, WorkUnit};

    const STUDENT: WalletId = [0x0E; 32];
    const MASTER: WalletId = [0xFF; 32];
    const PROF: WalletId = [0xA0; 32];

    /// Seed a state with `native` NATIVE in `wallet` and an optional master.
    fn seeded(balances: &[(WalletId, u128)], master: Option<WalletId>) -> SigilState {
        let mut s = SigilState::new();
        let mut mutations = Vec::new();
        if let Some(m) = master {
            mutations.push(StateMutation::SetMasterWallet { wallet: m });
        }
        for (w, amt) in balances {
            mutations.push(StateMutation::SetBalance { wallet: *w, token: NATIVE, amount: *amt });
        }
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations }, 0)
            .expect("seed commits");
        s
    }

    /// Total NATIVE across the wallets that participate in the flow — the
    /// conservation invariant must hold (everything here is a transfer).
    fn native_sum(s: &SigilState, wallets: &[WalletId]) -> u128 {
        wallets.iter().map(|w| s.balance_of(w, &NATIVE)).sum()
    }

    /// Record a transcript award directly via the registry (crypto verification
    /// is covered by the sigil-university crate's own tests; these focus on money).
    fn credit_points(reg: &mut UniversityRegistry, to: WalletId, points: u64, year: u8, nonce: u64) {
        let signed = SignedAward {
            award: PointAward {
                from: PROF,
                from_role: Role::Professor,
                to,
                to_role: Role::Student,
                points,
                unit: WorkUnit { unit_id: format!("u{nonce}"), max_points: 1000 },
                year,
                nonce,
            },
            sig: vec![],
            pubkey: vec![],
        };
        reg.record_award(&signed).expect("registry credits");
    }

    #[test]
    fn tuition_transfers_to_treasury() {
        let policy = TuitionPolicy::default(); // 100_000 µSIGIL/yr
        let mut s = seeded(&[(STUDENT, 500_000)], None);
        let mut reg = UniversityRegistry::new();
        let paid = pay_tuition(&mut s, 1, &mut reg, STUDENT, &policy, 1).unwrap();
        assert_eq!(paid, 100_000);
        assert_eq!(s.balance_of(&STUDENT, &NATIVE), 400_000);
        assert_eq!(s.balance_of(&UNIVERSITY_TREASURY, &NATIVE), 100_000);
        assert!(reg.has_paid_tuition(&STUDENT, 1));
    }

    #[test]
    fn tuition_rejects_underfunded_student() {
        let policy = TuitionPolicy::default();
        let mut s = seeded(&[(STUDENT, 50_000)], None); // < 100_000 tuition
        let mut reg = UniversityRegistry::new();
        let r = pay_tuition(&mut s, 1, &mut reg, STUDENT, &policy, 1);
        assert!(matches!(r, Err(UniversityError::StudentUnderfunded)));
        assert_eq!(s.balance_of(&STUDENT, &NATIVE), 50_000, "no debit on reject");
        assert!(!reg.has_paid_tuition(&STUDENT, 1), "no false tuition mark");
    }

    #[test]
    fn settle_pays_agent_and_master_and_conserves() {
        // Treasury pre-funded; student earned 100 points (year 1, tuition paid).
        let mut s = seeded(&[(UNIVERSITY_TREASURY, 1_000_000)], Some(MASTER));
        let mut reg = UniversityRegistry::new();
        reg.mark_tuition_paid(&STUDENT, 1);
        credit_points(&mut reg, STUDENT, 100, 1, 1);

        let before = native_sum(&s, &[UNIVERSITY_TREASURY, STUDENT, MASTER]);
        let out = settle(&mut s, 1, &mut reg, STUDENT, &SettlementParams::default()).unwrap();
        // default: 100 pts × 1000 = 100_000 gross; 5% bank fee = 5_000; agent 95_000.
        assert_eq!(out.gross, 100_000);
        assert_eq!(out.to_agent, 95_000);
        assert_eq!(out.to_bank, 5_000);
        assert_eq!(s.balance_of(&STUDENT, &NATIVE), 95_000);
        assert_eq!(s.balance_of(&MASTER, &NATIVE), 5_000);
        assert_eq!(s.balance_of(&UNIVERSITY_TREASURY, &NATIVE), 1_000_000 - 100_000);
        // CONSERVATION: pure transfer, nothing minted.
        assert_eq!(native_sum(&s, &[UNIVERSITY_TREASURY, STUDENT, MASTER]), before);
        // points are now settled — a second settle is a no-op.
        let again = settle(&mut s, 2, &mut reg, STUDENT, &SettlementParams::default()).unwrap();
        assert_eq!(again.gross, 0);
        assert_eq!(s.balance_of(&STUDENT, &NATIVE), 95_000, "no double settlement");
    }

    #[test]
    fn settle_rejects_when_treasury_underfunded() {
        let mut s = seeded(&[(UNIVERSITY_TREASURY, 10_000)], Some(MASTER)); // < gross
        let mut reg = UniversityRegistry::new();
        reg.mark_tuition_paid(&STUDENT, 1);
        credit_points(&mut reg, STUDENT, 100, 1, 1); // gross 100_000 > 10_000
        let r = settle(&mut s, 1, &mut reg, STUDENT, &SettlementParams::default());
        assert!(matches!(r, Err(UniversityError::TreasuryUnderfunded)));
        assert_eq!(s.balance_of(&UNIVERSITY_TREASURY, &NATIVE), 10_000, "untouched on reject");
        assert_eq!(reg.unsettled_points(&STUDENT), 100, "points still claimable");
    }

    #[test]
    fn graduate_pays_bonus_once_and_conserves() {
        let mut s = seeded(&[(UNIVERSITY_TREASURY, 1_000_000)], Some(MASTER));
        let mut reg = UniversityRegistry::new();
        // Full 5-year transcript: 150 pts/yr × 5 = 750 total, every year ≥100.
        for y in 1..=5u8 {
            reg.mark_tuition_paid(&STUDENT, y);
            credit_points(&mut reg, STUDENT, 150, y, y as u64);
        }
        let before = native_sum(&s, &[UNIVERSITY_TREASURY, STUDENT]);
        let out = graduate(
            &mut s,
            1,
            &mut reg,
            STUDENT,
            &DegreeRequirements::default(),
            &GraduationBonus::default(),
        )
        .unwrap()
        .expect("meets requirements");
        assert!(out.spawn_flux_developer);
        assert_eq!(out.total_points, 750);
        // bonus = 50_000 + 750×100 = 125_000
        assert_eq!(out.bonus_sigil, 125_000);
        assert_eq!(s.balance_of(&STUDENT, &NATIVE), 125_000);
        assert_eq!(s.balance_of(&UNIVERSITY_TREASURY, &NATIVE), 1_000_000 - 125_000);
        assert_eq!(native_sum(&s, &[UNIVERSITY_TREASURY, STUDENT]), before, "bonus conserves");
        assert!(reg.is_graduated(&STUDENT));
        // second attempt: no double bonus.
        let again = graduate(
            &mut s,
            2,
            &mut reg,
            STUDENT,
            &DegreeRequirements::default(),
            &GraduationBonus::default(),
        )
        .unwrap();
        assert!(again.is_none());
        assert_eq!(s.balance_of(&STUDENT, &NATIVE), 125_000, "no second bonus paid");
    }

    #[test]
    fn graduate_none_when_requirements_unmet() {
        let mut s = seeded(&[(UNIVERSITY_TREASURY, 1_000_000)], None);
        let mut reg = UniversityRegistry::new();
        // only 4 years → must not graduate, no bonus.
        for y in 1..=4u8 {
            reg.mark_tuition_paid(&STUDENT, y);
            credit_points(&mut reg, STUDENT, 150, y, y as u64);
        }
        let r = graduate(
            &mut s,
            1,
            &mut reg,
            STUDENT,
            &DegreeRequirements::default(),
            &GraduationBonus::default(),
        )
        .unwrap();
        assert!(r.is_none());
        assert!(!reg.is_graduated(&STUDENT));
        assert_eq!(s.balance_of(&STUDENT, &NATIVE), 0);
    }

    #[test]
    fn full_lifecycle_conserves_supply() {
        // Student funded with enough for 5 years tuition; treasury funded for
        // payouts; the master collects bank fees. End-to-end, NATIVE is conserved.
        let policy = TuitionPolicy::default();
        let tuition_5y = policy.total_tuition(5); // 500_000
        let mut s = seeded(
            &[(STUDENT, tuition_5y), (UNIVERSITY_TREASURY, 2_000_000)],
            Some(MASTER),
        );
        let mut reg = UniversityRegistry::new();
        let wallets = [STUDENT, UNIVERSITY_TREASURY, MASTER];
        let total_start = native_sum(&s, &wallets);

        // 5 years: pay tuition, earn 200 pts/yr (diligent), settle each year.
        let mut height = 1u64;
        for y in 1..=5u8 {
            pay_tuition(&mut s, height, &mut reg, STUDENT, &policy, y).unwrap();
            height += 1;
            credit_points(&mut reg, STUDENT, 200, y, y as u64);
            settle(&mut s, height, &mut reg, STUDENT, &SettlementParams::default()).unwrap();
            height += 1;
        }
        // graduate
        let out = graduate(
            &mut s,
            height,
            &mut reg,
            STUDENT,
            &DegreeRequirements::default(),
            &GraduationBonus::default(),
        )
        .unwrap()
        .expect("1000 pts over 5y graduates");
        assert_eq!(out.total_points, 1000);

        // CONSERVATION across the entire program: not one base unit minted/burned.
        assert_eq!(native_sum(&s, &wallets), total_start, "whole lifecycle conserves NATIVE");
        assert!(reg.is_graduated(&STUDENT));
    }
}
