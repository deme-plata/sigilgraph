//! monitor.rs — MandatPilot product #2: CVR-Overvågning (company monitoring).
//! Same engine (credit ledger), new shell. Watch a company; each check debits a small
//! fee, re-reads the open CVR register, and diffs against the last snapshot. Creditors,
//! suppliers and B2B sellers pay for the peace of mind — the data is free.

use crate::{debit_action, MandatError};
use flux_uint::Amount;
use sigil_state::{SigilState, StateTransition, WalletId};

/// 2 credits per check (cheaper than a full verify).
pub const MONITOR_COST: Amount = Amount::from_ore(2);
/// 1 credit to start watching (stores the baseline snapshot).
pub const WATCH_COST: Amount = Amount::from_ore(1);

/// A point-in-time view of a company from the open register (cvrapi.dk / Virk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CvrSnapshot {
    pub cvr: String,
    pub company_name: String,
    pub active: bool,
    pub bankrupt: bool, // cvrapi `creditbankrupt` — the konkurs alarm
    pub employees: u32,
}

/// What changed between two snapshots — the alertable events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    NameChanged { from: String, to: String },
    StatusChanged { now_active: bool },     // active → inactive = tvangsopløsning signal
    BankruptcyChanged { now_bankrupt: bool }, // ⚠ the big one — went into konkurs
    EmployeesChanged { from: u32, to: u32 },
}

/// Pure diff — the alert logic. Empty vec = nothing changed.
pub fn diff(prev: &CvrSnapshot, now: &CvrSnapshot) -> Vec<Change> {
    let mut out = Vec::new();
    if prev.company_name != now.company_name {
        out.push(Change::NameChanged { from: prev.company_name.clone(), to: now.company_name.clone() });
    }
    if prev.active != now.active {
        out.push(Change::StatusChanged { now_active: now.active });
    }
    if prev.bankrupt != now.bankrupt {
        out.push(Change::BankruptcyChanged { now_bankrupt: now.bankrupt });
    }
    if prev.employees != now.employees {
        out.push(Change::EmployeesChanged { from: prev.employees, to: now.employees });
    }
    out
}

/// Start watching: charge WATCH_COST. The caller stores the baseline snapshot.
pub fn watch_start(state: &SigilState, account: &WalletId, at_height: u64) -> Result<StateTransition, MandatError> {
    debit_action(state, account, WATCH_COST, at_height)
}

/// Run a check: charge MONITOR_COST, diff prev vs now, return the changes. Insufficient
/// credits blocks (no charge, no check). The caller persists `now` as the new baseline.
pub fn monitor_check(
    state: &SigilState,
    account: &WalletId,
    prev: &CvrSnapshot,
    now: &CvrSnapshot,
    at_height: u64,
) -> Result<(StateTransition, Vec<Change>), MandatError> {
    let t = debit_action(state, account, MONITOR_COST, at_height)?;
    Ok((t, diff(prev, now)))
}
