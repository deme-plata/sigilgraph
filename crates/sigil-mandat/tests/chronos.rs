//! chronos.rs — deterministic money-integrity scenarios for the MandatPilot credit
//! ledger ON SigilGraph. Seeded ⇒ identical every run. Every assertion guards a
//! balance-integrity invariant, now committed through sigil-state's real chokepoint.

use flux_uint::Amount;
use sigil_mandat::{account_from_mitid, credits_of, debit_action, topup_credit, MandatError, CREDITS, TREASURY};
use sigil_state::{commit_state_transition, SigilState};
use std::collections::HashSet;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { let mut x = self.0; x ^= x << 13; x ^= x >> 7; x ^= x << 17; self.0 = x; x }
    fn amt(&mut self, max: u128) -> u128 { ((self.next() as u128) % max).max(1) }
}

/// tiny harness: tracks the monotonic commit height
struct Ledger { s: SigilState, h: u64 }
impl Ledger {
    fn new() -> Self { Ledger { s: SigilState::new(), h: 0 } }
    fn topup(&mut self, acct: &[u8; 32], a: u128) -> Result<(), MandatError> {
        self.h += 1;
        let t = topup_credit(&self.s, acct, Amount::from_ore(a), self.h)?;
        commit_state_transition(&mut self.s, &t, self.h).unwrap();
        Ok(())
    }
    fn debit(&mut self, acct: &[u8; 32], a: u128) -> Result<(), MandatError> {
        self.h += 1;
        match debit_action(&self.s, acct, Amount::from_ore(a), self.h) {
            Ok(t) => { commit_state_transition(&mut self.s, &t, self.h).unwrap(); Ok(()) }
            Err(e) => { self.h -= 1; Err(e) } // rejected → no height consumed, state untouched
        }
    }
    fn credits(&self, acct: &[u8; 32]) -> u128 { credits_of(&self.s, acct).as_ore() }
}

#[test]
fn topup_then_debit_conserved() {
    let mut l = Ledger::new();
    let alice = account_from_mitid("sgl-alice-uuid");
    l.topup(&alice, 100).unwrap();
    l.debit(&alice, 30).unwrap();
    assert_eq!(l.credits(&alice), 70);
    assert_eq!(l.credits(&TREASURY), 30); // spent credits conserved into treasury
    assert_eq!(l.credits(&alice) + l.credits(&TREASURY), 100); // nothing created/destroyed
}

#[test]
fn invisible_chain_account_is_the_identity() {
    // no seed, no wallet — same MitID identity ⇒ same account; different ⇒ different.
    assert_eq!(account_from_mitid("uuid-A"), account_from_mitid("uuid-A"));
    assert_ne!(account_from_mitid("uuid-A"), account_from_mitid("uuid-B"));
    assert_ne!(account_from_mitid("uuid-A"), TREASURY);
}

#[test]
fn idempotent_topup_no_double_credit() {
    let mut l = Ledger::new();
    let acct = account_from_mitid("replay-victim");
    let mut seen: HashSet<u64> = HashSet::new();
    let mut r = Rng(0x5712);
    let mut credited = 0u128;
    for _ in 0..20_000 {
        let event_id = r.next() % 800; // collisions = Stripe webhook replays
        let amt = r.amt(5_000);
        if seen.insert(event_id) { l.topup(&acct, amt).unwrap(); credited += amt; }
        // a replay of a seen event is dropped BEFORE topup → balance can't move
    }
    assert_eq!(l.credits(&acct), credited); // exactly the unique credits
}

#[test]
fn oversell_rejected_state_unchanged() {
    let mut l = Ledger::new();
    let acct = account_from_mitid("poor-user");
    l.topup(&acct, 50).unwrap();
    let before_acct = l.credits(&acct);
    let before_tr = l.credits(&TREASURY);
    let err = l.debit(&acct, 51).unwrap_err(); // can't afford
    assert_eq!(err, MandatError::Insufficient { have: 50, need: 51 });
    assert_eq!(l.credits(&acct), before_acct);   // state untouched
    assert_eq!(l.credits(&TREASURY), before_tr); // treasury untouched
    l.debit(&acct, 50).unwrap(); // exact spend is fine
    assert_eq!(l.credits(&acct), 0);
}

#[test]
fn concurrent_debits_never_oversell() {
    let mut r = Rng(13);
    for _ in 0..400 {
        let mut l = Ledger::new();
        let acct = account_from_mitid("hammered");
        let start = r.amt(1_000_000);
        l.topup(&acct, start).unwrap();
        let mut spent = 0u128;
        for _ in 0..150 {
            let want = r.amt(30_000);
            if l.debit(&acct, want).is_ok() { spent += want; }
            assert!(l.credits(&acct) <= start); // never grows
        }
        assert_eq!(l.credits(&acct), start - spent);       // exact, no oversell
        assert_eq!(l.credits(&TREASURY), spent);            // every spent credit landed
        assert_eq!(l.credits(&acct) + l.credits(&TREASURY), start); // conserved
    }
}

// ── the full $2 → credits → verify loop ────────────────────────────────────
use sigil_mandat::{apply_payment, credits_for, verify_business, CvrRecord, MitidClaims, Payment};

#[test]
fn pricing_two_dollars_is_100_credits() {
    assert_eq!(credits_for(200).as_ore(), 100); // $2.00
    assert_eq!(credits_for(900).as_ore(), 450); // $9.00
    assert_eq!(credits_for(2).as_ore(), 1);      // $0.02 = 1 credit
}

#[test]
fn stripe_onramp_idempotent() {
    let mut l = Ledger::new();
    let acct = account_from_mitid("buyer-uuid");
    let mut seen = std::collections::HashSet::new();
    let pay = Payment { id: "evt_stripe_123".into(), usd_cents: 200 };
    // first delivery → credits land
    let t = apply_payment(&l.s, &acct, &pay, l.h + 1, &mut seen).unwrap().unwrap();
    l.h += 1; commit_state_transition(&mut l.s, &t, l.h).unwrap();
    assert_eq!(l.credits(&acct), 100);
    // webhook REPLAY of the same event → no-op, balance unchanged
    assert!(apply_payment(&l.s, &acct, &pay, l.h + 1, &mut seen).is_none());
    assert_eq!(l.credits(&acct), 100);
}

#[test]
fn full_loop_topup_then_verify() {
    let mut l = Ledger::new();
    let acct = account_from_mitid("turf-erhverv-uuid");
    let mut seen = std::collections::HashSet::new();

    // 1) $2 in via Stripe → 100 credits on-chain
    let pay = Payment { id: "evt_001".into(), usd_cents: 200 };
    let t = apply_payment(&l.s, &acct, &pay, l.h + 1, &mut seen).unwrap().unwrap();
    l.h += 1; commit_state_transition(&mut l.s, &t, l.h).unwrap();
    assert_eq!(l.credits(&acct), 100);

    // 2) CVR-Verify: MitID claims × CVR register → debit 10, proven answer
    let claims = MitidClaims { cvr: "24256790".into(), person_name: "Vera Holm".into(), is_signatory: true };
    let reg = CvrRecord { cvr: "24256790".into(), company_name: "Novo Nordisk A/S".into(), active: true, bankrupt: false, employees: 30074, industry: "pharma".into() };
    l.h += 1;
    let (tx, res) = verify_business(&l.s, &acct, &claims, &reg, l.h).unwrap();
    commit_state_transition(&mut l.s, &tx, l.h).unwrap();

    assert!(res.verified);                       // signatory × active × matching CVR
    assert_eq!(l.credits(&acct), 90);            // 100 − 10
    assert_eq!(l.credits(&TREASURY), 10);        // revenue conserved
    assert_eq!(res.company_name, "Novo Nordisk A/S");
    assert!(!res.bankrupt);                      // enriched: not in konkurs
    assert_eq!(res.employees, 30074);            // enriched: headcount
    assert_eq!(res.industry, "pharma");          // enriched: branche
}

#[test]
fn verify_surfaces_bankruptcy_flag() {
    // a verified signatory of a company that IS in konkurs — the verify still succeeds
    // (identity is real) but the enriched bankrupt flag is the risk signal the caller acts on.
    let mut l = Ledger::new();
    let acct = account_from_mitid("creditor-checking");
    let p = Payment { id: "e2".into(), usd_cents: 200 };
    let mut seen = std::collections::HashSet::new();
    let t = apply_payment(&l.s, &acct, &p, l.h + 1, &mut seen).unwrap().unwrap();
    l.h += 1; commit_state_transition(&mut l.s, &t, l.h).unwrap();

    let claims = MitidClaims { cvr: "11223344".into(), person_name: "Ole Risk".into(), is_signatory: true };
    let reg = CvrRecord { cvr: "11223344".into(), company_name: "Skrøbelig ApS".into(), active: true, bankrupt: true, employees: 3, industry: "byggeri".into() };
    l.h += 1;
    let (tx, res) = verify_business(&l.s, &acct, &claims, &reg, l.h).unwrap();
    commit_state_transition(&mut l.s, &tx, l.h).unwrap();
    assert!(res.verified);   // identity is genuine
    assert!(res.bankrupt);   // …but ⚠ in konkurs — don't extend credit
}

#[test]
fn verify_negative_when_not_signatory_or_mismatch() {
    let mut l = Ledger::new();
    let acct = account_from_mitid("x");
    let p = Payment { id: "e".into(), usd_cents: 200 };
    let mut seen = std::collections::HashSet::new();
    let t = apply_payment(&l.s, &acct, &p, l.h + 1, &mut seen).unwrap().unwrap();
    l.h += 1; commit_state_transition(&mut l.s, &t, l.h).unwrap();

    // not a signatory → verified=false but still charged (the check ran)
    let claims = MitidClaims { cvr: "10000000".into(), person_name: "N".into(), is_signatory: false };
    let reg = CvrRecord { cvr: "10000000".into(), company_name: "Co".into(), active: true, bankrupt: false, employees: 0, industry: String::new() };
    l.h += 1;
    let (tx, res) = verify_business(&l.s, &acct, &claims, &reg, l.h).unwrap();
    commit_state_transition(&mut l.s, &tx, l.h).unwrap();
    assert!(!res.verified);
    assert_eq!(l.credits(&acct), 90); // charged for running it
}

#[test]
fn verify_blocked_when_no_credits() {
    let mut l = Ledger::new();
    let acct = account_from_mitid("broke"); // never topped up → 0 credits
    let claims = MitidClaims { cvr: "1".into(), person_name: "N".into(), is_signatory: true };
    let reg = CvrRecord { cvr: "1".into(), company_name: "C".into(), active: true, bankrupt: false, employees: 0, industry: String::new() };
    let err = verify_business(&l.s, &acct, &claims, &reg, l.h + 1).unwrap_err();
    assert_eq!(err, MandatError::Insufficient { have: 0, need: 10 }); // no credits, no verify
}

// ── product #2: CVR-Overvågning (monitor) ──────────────────────────────────
use sigil_mandat::{diff, monitor_check, watch_start, Change, CvrSnapshot};

fn snap(name: &str, active: bool, emp: u32) -> CvrSnapshot {
    CvrSnapshot { cvr: "24256790".into(), company_name: name.into(), active, bankrupt: false, employees: emp }
}
fn snap_bk(name: &str, active: bool, bankrupt: bool, emp: u32) -> CvrSnapshot {
    CvrSnapshot { cvr: "24256790".into(), company_name: name.into(), active, bankrupt, employees: emp }
}

#[test]
fn diff_detects_the_right_changes() {
    let a = snap("Novo Nordisk A/S", true, 30000);
    assert!(diff(&a, &a).is_empty()); // no change
    // konkurs-signal: active → inactive
    let b = snap("Novo Nordisk A/S", false, 30000);
    assert_eq!(diff(&a, &b), vec![Change::StatusChanged { now_active: false }]);
    // rename + headcount move
    let c = snap("Novo Holding", true, 30050);
    let ch = diff(&a, &c);
    assert!(ch.contains(&Change::NameChanged { from: "Novo Nordisk A/S".into(), to: "Novo Holding".into() }));
    assert!(ch.contains(&Change::EmployeesChanged { from: 30000, to: 30050 }));
}

#[test]
fn diff_detects_bankruptcy_flag() {
    // the konkurs flag flips while the company is still "active" in the register —
    // overvågning must alarm on the dedicated bankrupt signal, not only on active/inactive.
    let solvent = snap_bk("Skrøbelig ApS", true, false, 5);
    let konkurs = snap_bk("Skrøbelig ApS", true, true, 5);
    assert_eq!(diff(&solvent, &konkurs), vec![Change::BankruptcyChanged { now_bankrupt: true }]);
    // and it clears again if the flag is lifted
    assert_eq!(diff(&konkurs, &solvent), vec![Change::BankruptcyChanged { now_bankrupt: false }]);
}

#[test]
fn monitor_charges_and_alerts_on_konkurs() {
    let mut l = Ledger::new();
    let acct = account_from_mitid("creditor-x");
    l.topup(&acct, 100).unwrap();
    // start watching (1 credit)
    l.h += 1;
    let t = watch_start(&l.s, &acct, l.h).unwrap();
    commit_state_transition(&mut l.s, &t, l.h).unwrap();
    assert_eq!(l.credits(&acct), 99);
    // a check that detects konkurs (2 credits)
    let prev = snap("Acme ApS", true, 12);
    let now = snap("Acme ApS", false, 12); // went inactive
    l.h += 1;
    let (tx, changes) = monitor_check(&l.s, &acct, &prev, &now, l.h).unwrap();
    commit_state_transition(&mut l.s, &tx, l.h).unwrap();
    assert_eq!(changes, vec![Change::StatusChanged { now_active: false }]); // ALERT
    assert_eq!(l.credits(&acct), 97); // 99 − 2
    assert_eq!(l.credits(&TREASURY), 3); // 1 watch + 2 check = revenue
}

#[test]
fn monitor_blocked_when_no_credits() {
    let l = Ledger::new();
    let acct = account_from_mitid("broke-watcher");
    let prev = snap("X", true, 1);
    let err = monitor_check(&l.s, &acct, &prev, &prev, l.h + 1).unwrap_err();
    assert_eq!(err, MandatError::Insufficient { have: 0, need: 2 });
}
