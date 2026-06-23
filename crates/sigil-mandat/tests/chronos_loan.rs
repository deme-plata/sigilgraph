//! chronos_loan.rs — deterministic money-integrity scenarios for the Quillon Bank credit
//! layer (credit_line.rs) ON SigilGraph. Seeded ⇒ identical every run. Every assertion
//! guards one of the invariants I1–I7 from MANDAT_QUILLON_BANK_KREDIT.md, all committed
//! through sigil-state's real chokepoint. Propose-only: this proves integrity BEFORE any
//! real QUGUSD loan is ever requested.

use flux_uint::Amount;
use sigil_mandat::{
    account_from_mitid, advance, borrow_against_collateral, collateral_of, credits_of, debt_of,
    liquidate, repay, repay_and_release, topup_credit, LoanError, COLLAT, CREDITS, TREASURY, VAULT,
};
use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, WalletId, NATIVE};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { let mut x = self.0; x ^= x << 13; x ^= x >> 7; x ^= x << 17; self.0 = x; x }
    fn amt(&mut self, max: u128) -> u128 { ((self.next() as u128) % max).max(1) }
}

/// tiny harness: monotonic commit height + the bank-backed treasury float seeding.
struct Ledger { s: SigilState, h: u64 }
impl Ledger {
    fn new() -> Self { Ledger { s: SigilState::new(), h: 0 } }

    fn commit(&mut self, t: StateTransition) { commit_state_transition(&mut self.s, &t, self.h).unwrap(); }

    /// Seed CREDITS (Stripe topup, or — for TREASURY — the QUGUSD-loan-backed float).
    fn topup(&mut self, acct: &WalletId, a: u128) {
        self.h += 1;
        let t = topup_credit(&self.s, acct, Amount::from_ore(a), self.h).unwrap();
        self.commit(t);
    }
    /// Seed an agent's existing NATIVE holdings (genesis-distributed QUG equivalent).
    fn seed_native(&mut self, acct: &WalletId, a: u128) {
        self.h += 1;
        let t = StateTransition { at_height: self.h, mutations: vec![
            StateMutation::SetBalance { wallet: *acct, token: NATIVE, amount: a },
        ] };
        self.commit(t);
    }

    fn advance(&mut self, acct: &WalletId, a: u128, limit: u128) -> Result<(), LoanError> {
        self.h += 1;
        match advance(&self.s, acct, Amount::from_ore(a), Amount::from_ore(limit), self.h) {
            Ok(t) => { self.commit(t); Ok(()) }
            Err(e) => { self.h -= 1; Err(e) }
        }
    }
    fn repay(&mut self, acct: &WalletId, a: u128) -> Result<(), LoanError> {
        self.h += 1;
        match repay(&self.s, acct, Amount::from_ore(a), self.h) {
            Ok(t) => { self.commit(t); Ok(()) }
            Err(e) => { self.h -= 1; Err(e) }
        }
    }
    fn borrow(&mut self, agent: &WalletId, collateral: u128, ltv_bps: u32) -> Result<(), LoanError> {
        self.h += 1;
        match borrow_against_collateral(&self.s, agent, Amount::from_ore(collateral), ltv_bps, self.h) {
            Ok(t) => { self.commit(t); Ok(()) }
            Err(e) => { self.h -= 1; Err(e) }
        }
    }
    fn repay_release(&mut self, agent: &WalletId, a: u128) -> Result<(), LoanError> {
        self.h += 1;
        match repay_and_release(&self.s, agent, Amount::from_ore(a), self.h) {
            Ok(t) => { self.commit(t); Ok(()) }
            Err(e) => { self.h -= 1; Err(e) }
        }
    }
    fn liquidate(&mut self, agent: &WalletId) -> Result<(), LoanError> {
        self.h += 1;
        match liquidate(&self.s, agent, self.h) {
            Ok(t) => { self.commit(t); Ok(()) }
            Err(e) => { self.h -= 1; Err(e) }
        }
    }

    fn credits(&self, a: &WalletId) -> u128 { credits_of(&self.s, a).as_ore() }
    fn debt(&self, a: &WalletId) -> u128 { debt_of(&self.s, a).as_ore() }
    fn collat(&self, a: &WalletId) -> u128 { collateral_of(&self.s, a).as_ore() }
    fn native(&self, a: &WalletId) -> u128 { self.s.balance_of(a, &NATIVE) }
    fn supply(&self) -> u128 { self.s.native_supply() }
}

// ── BORGER layer — treasury-backed "Betal senere" ───────────────────────────

#[test]
fn advance_records_matching_debt_i1() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000); // bank-backed float
    let borger = account_from_mitid("borger-uuid");
    l.advance(&borger, 150, 200).unwrap();
    assert_eq!(l.credits(&borger), 150);                 // got the credits
    assert_eq!(l.debt(&borger), 150);                    // I1: equal debt recorded
    assert_eq!(l.credits(&TREASURY), 850);               // float drawn down
    assert_eq!(l.credits(&borger) + l.credits(&TREASURY), 1_000); // CREDITS conserved
}

#[test]
fn advance_over_limit_rejected_i2() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let borger = account_from_mitid("limited");
    l.advance(&borger, 150, 200).unwrap();
    let err = l.advance(&borger, 60, 200).unwrap_err();  // 150+60=210 > 200
    assert_eq!(err, LoanError::OverLimit { would_be: 210, limit: 200 });
    assert_eq!(l.debt(&borger), 150);                    // state untouched on reject
    assert_eq!(l.credits(&borger), 150);
    l.advance(&borger, 50, 200).unwrap();                // exactly to the limit is fine
    assert_eq!(l.debt(&borger), 200);
}

#[test]
fn advance_blocked_when_float_dry() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 100);                             // tiny float
    let borger = account_from_mitid("early-bird");
    let err = l.advance(&borger, 150, 500).unwrap_err(); // limit ok, but float can't cover
    assert_eq!(err, LoanError::TreasuryDry { have: 100, need: 150 });
    assert_eq!(l.credits(&borger), 0);
}

#[test]
fn repay_clears_debt_returns_float_i5() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let borger = account_from_mitid("good-payer");
    l.advance(&borger, 150, 200).unwrap();
    l.repay(&borger, 150).unwrap();
    assert_eq!(l.debt(&borger), 0);                      // debt cleared
    assert_eq!(l.credits(&borger), 0);
    assert_eq!(l.credits(&TREASURY), 1_000);             // float fully restored
}

#[test]
fn over_repay_rejected_i5() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let borger = account_from_mitid("over-payer");
    l.advance(&borger, 100, 200).unwrap();
    let err = l.repay(&borger, 101).unwrap_err();
    assert_eq!(err, LoanError::OverRepay { owe: 100, tried: 101 });
    assert_eq!(l.debt(&borger), 100);                    // unchanged
}

#[test]
fn cannot_repay_credits_already_spent() {
    use sigil_mandat::debit_action;
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let borger = account_from_mitid("spender");
    l.advance(&borger, 100, 200).unwrap();               // 100 credits, 100 debt
    // spend 30 credits on a product action (verify/monitor) → routed to treasury
    l.h += 1;
    let t = debit_action(&l.s, &borger, Amount::from_ore(30), l.h).unwrap();
    l.commit(t);
    assert_eq!(l.credits(&borger), 70);
    let err = l.repay(&borger, 100).unwrap_err();        // owes 100 but only holds 70
    assert_eq!(err, LoanError::CannotPay { have: 70, need: 100 });
}

// ── AGENT layer — crypto-native, own NATIVE as collateral ────────────────────

#[test]
fn agent_borrow_locks_collateral_conserves_native_i3_i4_i6() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let agent = account_from_mitid("codex-agent");
    l.seed_native(&agent, 1_000);
    let supply0 = l.supply();

    l.borrow(&agent, 600, 6600).unwrap();                // 600 collateral @ 66% → 396 credits
    assert_eq!(l.credits(&agent), 396);
    assert_eq!(l.debt(&agent), 396);                     // I1 holds here too
    assert_eq!(l.native(&agent), 400);                   // 1000 − 600 locked
    assert_eq!(l.native(&VAULT), 600);                   // I3: collateral in the vault
    assert_eq!(l.collat(&agent), 600);                   // I4: COLLAT mirrors VAULT
    assert_eq!(l.collat(&agent), l.native(&VAULT));
    assert_eq!(l.native(&agent) + l.native(&VAULT), 1_000); // NATIVE conserved
    assert_eq!(l.supply(), supply0);                     // I6: cap sacred — no mint
    assert_eq!(l.credits(&TREASURY), 604);               // float drawn by 396
}

#[test]
fn agent_full_repay_releases_collateral() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let agent = account_from_mitid("repayer");
    l.seed_native(&agent, 1_000);
    l.borrow(&agent, 600, 6600).unwrap();                // → 396 credits/debt
    l.repay_release(&agent, 396).unwrap();               // full payback
    assert_eq!(l.debt(&agent), 0);
    assert_eq!(l.native(&agent), 1_000);                 // collateral fully released
    assert_eq!(l.native(&VAULT), 0);
    assert_eq!(l.collat(&agent), 0);
    assert_eq!(l.credits(&TREASURY), 1_000);             // float restored
}

#[test]
fn agent_partial_repay_keeps_collateral() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let agent = account_from_mitid("partial");
    l.seed_native(&agent, 1_000);
    l.borrow(&agent, 600, 6600).unwrap();
    l.repay_release(&agent, 100).unwrap();               // partial — debt 396 → 296
    assert_eq!(l.debt(&agent), 296);
    assert_eq!(l.native(&VAULT), 600);                   // collateral STILL locked
    assert_eq!(l.collat(&agent), 600);
    assert_eq!(l.native(&agent), 400);
}

#[test]
fn ltv_above_max_rejected_i2() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let agent = account_from_mitid("greedy");
    l.seed_native(&agent, 1_000);
    let err = l.borrow(&agent, 600, 7_600).unwrap_err(); // 76% > 75% cap
    assert_eq!(err, LoanError::LtvTooHigh { bps: 7_600, max: 7_500 });
    assert_eq!(l.native(&VAULT), 0);                     // nothing locked
    assert_eq!(l.debt(&agent), 0);
}

#[test]
fn liquidate_seizes_collateral_net_sum_i7() {
    let mut l = Ledger::new();
    l.topup(&TREASURY, 1_000);
    let agent = account_from_mitid("defaulter");
    l.seed_native(&agent, 1_000);
    let supply0 = l.supply();
    l.borrow(&agent, 600, 6600).unwrap();                // agent walks off with 396 credits
    l.liquidate(&agent).unwrap();                        // default → seize collateral
    assert_eq!(l.collat(&agent), 0);
    assert_eq!(l.debt(&agent), 0);
    assert_eq!(l.native(&VAULT), 0);
    assert_eq!(l.native(&TREASURY), 600);                // I7: seized collateral covers the loss
    assert_eq!(l.native(&agent) + l.native(&TREASURY), 1_000); // NATIVE conserved
    assert_eq!(l.supply(), supply0);                     // cap sacred
}

#[test]
fn liquidate_without_loan_rejected() {
    let mut l = Ledger::new();
    let agent = account_from_mitid("not-a-borrower");
    let err = l.liquidate(&agent).unwrap_err();
    assert_eq!(err, LoanError::NoCollateral);
}

// ── fuzz: the global invariants must hold across any sequence ────────────────

#[test]
fn fuzz_collat_mirrors_vault_and_cap_sacred() {
    let mut r = Rng(0xC0FFEE);
    for trial in 0..200u64 {
        let mut l = Ledger::new();
        l.topup(&TREASURY, 10_000_000); // deep bank-backed float
        // a handful of agents, each with their own NATIVE
        let agents: Vec<WalletId> = (0..4)
            .map(|i| account_from_mitid(&format!("fuzz-{trial}-{i}")))
            .collect();
        for a in &agents { l.seed_native(a, 1_000_000); }
        let supply0 = l.supply();

        for _ in 0..120 {
            let a = &agents[(r.next() as usize) % agents.len()];
            match r.next() % 3 {
                0 => {
                    // borrow only if no live loan (one loan per agent in this model)
                    if l.debt(a) == 0 && l.collat(a) == 0 {
                        let coll = r.amt(900_000);
                        let _ = l.borrow(a, coll, 6600);
                    }
                }
                1 => {
                    // full repay (release) if solvent enough
                    let owe = l.debt(a);
                    if owe > 0 && l.credits(a) >= owe {
                        let _ = l.repay_release(a, owe);
                    }
                }
                _ => {
                    // liquidate a live loan
                    if l.collat(a) > 0 { let _ = l.liquidate(a); }
                }
            }

            // I4: Σ COLLAT over agents == NATIVE actually sitting in the VAULT.
            let sum_collat: u128 = agents.iter().map(|a| l.collat(a)).sum();
            assert_eq!(sum_collat, l.native(&VAULT), "I4 broken: COLLAT != VAULT");
            // I6: no loan op ever mints/burns NATIVE.
            assert_eq!(l.supply(), supply0, "I6 broken: native supply moved");
        }
    }
}

#[test]
fn fuzz_borger_credits_conserved() {
    let mut r = Rng(0xBEEF);
    let mut l = Ledger::new();
    let float0 = 1_000_000u128;
    l.topup(&TREASURY, float0);
    let borgere: Vec<WalletId> = (0..6).map(|i| account_from_mitid(&format!("b{i}"))).collect();
    let limit = 50_000u128;

    for _ in 0..5_000 {
        let b = &borgere[(r.next() as usize) % borgere.len()];
        if r.next() % 2 == 0 {
            let _ = l.advance(b, r.amt(20_000), limit);
        } else {
            let owe = l.debt(b);
            if owe > 0 { let _ = l.repay(b, r.amt(owe + 1)); }
        }
        // CREDITS are conserved: every credit is either in the float or out on a borger.
        let out: u128 = borgere.iter().map(|x| l.credits(x)).sum();
        assert_eq!(out + l.credits(&TREASURY), float0, "CREDITS not conserved");
        // and outstanding debt never exceeds the per-borger limit (I2).
        for x in &borgere { assert!(l.debt(x) <= limit, "I2 broken: debt over limit"); }
    }
}
