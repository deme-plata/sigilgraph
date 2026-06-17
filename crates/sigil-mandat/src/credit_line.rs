//! credit_line.rs — Quillon Bank kredit, lagdelt, ON SigilGraph, invisible to borgere.
//!
//! Two layers, ONE chokepoint (see MANDAT_QUILLON_BANK_KREDIT.md):
//!
//!   • BORGER ("Betal senere"): the treasury ADVANCES [`CREDITS`] to a MitID account up
//!     to an operator-set limit; every advanced credit records an equal [`DEBT`]. The
//!     treasury float is itself backed by ONE Quillon Bank QUGUSD loan (off-SIGIL,
//!     propose-only). The user sees "Kredit / Betal senere" — never QUG/wallet/collateral.
//!
//!   • AGENT (crypto-native): an agent locks its OWN [`NATIVE`] collateral into the
//!     [`VAULT`] and borrows `CREDITS = LTV × collateral`. [`repay_and_release`] returns
//!     the collateral on full payback; [`liquidate`] seizes it on default.
//!
//! Money safety: [`CREDITS`] and [`NATIVE`] are conserved — they MOVE between wallets,
//! never minted. [`DEBT`] / [`COLLAT`] are non-transferable bookkeeping claims, not money.
//! Every mutation is a typed [`StateTransition`] committed through sigil-state's chokepoint,
//! so the 21M cap and balance integrity hold by construction. Invariants are proven in
//! `tests/chronos_loan.rs` (I1–I7).

use crate::{credits_of, MandatError, CREDITS, TREASURY};
use flux_uint::Amount;
use sigil_state::{SigilState, StateMutation, StateTransition, TokenId, WalletId, NATIVE};

/// Per-account outstanding credit debt — a claim ledger, not a transferable token.
/// `balance_of(acct, DEBT)` = credits this account has been advanced and not yet repaid.
pub const DEBT: TokenId = *b"mandat-debt-v1-ledger-0000000001";

/// Per-account locked-collateral mirror — `balance_of(acct, COLLAT)` = how much NATIVE
/// this account currently has locked in [`VAULT`]. (The real NATIVE sits in VAULT;
/// COLLAT just remembers which account it belongs to.) Invariant I4: the sum of all
/// COLLAT balances always equals `balance_of(VAULT, NATIVE)`.
pub const COLLAT: TokenId = *b"mandat-collat-v1-ledger-00000001";

/// Where agent collateral is locked while a loan is open. NATIVE held here is "locked".
pub const VAULT: WalletId = *b"mandatpilot-collateral-vault-001";

/// Loan-to-Value in basis points. Matches Quillon Bank: default 66%, hard max 75%.
pub const DEFAULT_LTV_BPS: u32 = 6600;
pub const MAX_LTV_BPS: u32 = 7500;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LoanError {
    #[error("over credit limit: debt would be {would_be}, limit {limit}")]
    OverLimit { would_be: u128, limit: u128 },
    #[error("ltv too high: {bps} bps > max {max} bps")]
    LtvTooHigh { bps: u32, max: u32 },
    #[error("treasury float insufficient: have {have}, need {need}")]
    TreasuryDry { have: u128, need: u128 },
    #[error("repay exceeds debt: owe {owe}, tried {tried}")]
    OverRepay { owe: u128, tried: u128 },
    #[error("account lacks credits to repay: have {have}, need {need}")]
    CannotPay { have: u128, need: u128 },
    #[error("no collateral locked for this account")]
    NoCollateral,
    #[error("amount overflow")]
    Overflow,
    /// Re-export of the credit-ledger error so callers handle one type.
    #[error(transparent)]
    Mandat(#[from] MandatError),
}

/// Outstanding debt (advanced-but-unrepaid credits) for an account.
pub fn debt_of(state: &SigilState, account: &WalletId) -> Amount {
    Amount::from_ore(state.balance_of(account, &DEBT))
}

/// NATIVE collateral this account currently has locked in [`VAULT`].
pub fn collateral_of(state: &SigilState, account: &WalletId) -> Amount {
    Amount::from_ore(state.balance_of(account, &COLLAT))
}

/// `floor(amount × bps / 10_000)` — the borrowable credits against `collateral` at `ltv`.
fn apply_ltv(collateral: Amount, ltv_bps: u32) -> Option<Amount> {
    let c = collateral.as_ore();
    let scaled = c.checked_mul(ltv_bps as u128)?;
    Some(Amount::from_ore(scaled / 10_000))
}

// ── BORGER layer — treasury-backed "Betal senere" ───────────────────────────

/// Build the transition that ADVANCES `amount` credits from [`TREASURY`] to `account`
/// (drawing on the bank-backed float) and records an equal [`DEBT`]. Pure — emits nothing
/// and mutates nothing on rejection. (I1: advance == new debt; never free credits.)
pub fn advance(
    state: &SigilState,
    account: &WalletId,
    amount: Amount,
    limit: Amount,
    at_height: u64,
) -> Result<StateTransition, LoanError> {
    let cur_debt = debt_of(state, account);
    let new_debt = cur_debt.checked_add(amount).ok_or(LoanError::Overflow)?;
    // I2: outstanding debt may never exceed the limit.
    if new_debt.as_ore() > limit.as_ore() {
        return Err(LoanError::OverLimit { would_be: new_debt.as_ore(), limit: limit.as_ore() });
    }
    // The float must actually hold the credits we hand out.
    let float = credits_of(state, &TREASURY);
    let new_float = float.checked_sub(amount).ok_or(LoanError::TreasuryDry {
        have: float.as_ore(),
        need: amount.as_ore(),
    })?;
    let new_acct = credits_of(state, account).checked_add(amount).ok_or(LoanError::Overflow)?;
    Ok(StateTransition {
        at_height,
        mutations: vec![
            StateMutation::SetBalance { wallet: TREASURY, token: CREDITS, amount: new_float.as_ore() },
            StateMutation::SetBalance { wallet: *account, token: CREDITS, amount: new_acct.as_ore() },
            StateMutation::SetBalance { wallet: *account, token: DEBT, amount: new_debt.as_ore() },
        ],
    })
}

/// Build the transition that REPAYS `amount` credits: account → treasury, [`DEBT`] reduced.
/// (I5: repay ≤ debt, and the account must actually hold the credits it repays.)
pub fn repay(
    state: &SigilState,
    account: &WalletId,
    amount: Amount,
    at_height: u64,
) -> Result<StateTransition, LoanError> {
    let owe = debt_of(state, account);
    let new_debt = owe.checked_sub(amount).ok_or(LoanError::OverRepay {
        owe: owe.as_ore(),
        tried: amount.as_ore(),
    })?;
    let have = credits_of(state, account);
    let new_acct = have.checked_sub(amount).ok_or(LoanError::CannotPay {
        have: have.as_ore(),
        need: amount.as_ore(),
    })?;
    let new_float = credits_of(state, &TREASURY).checked_add(amount).ok_or(LoanError::Overflow)?;
    Ok(StateTransition {
        at_height,
        mutations: vec![
            StateMutation::SetBalance { wallet: *account, token: CREDITS, amount: new_acct.as_ore() },
            StateMutation::SetBalance { wallet: TREASURY, token: CREDITS, amount: new_float.as_ore() },
            StateMutation::SetBalance { wallet: *account, token: DEBT, amount: new_debt.as_ore() },
        ],
    })
}

// ── AGENT layer — crypto-native, own NATIVE as collateral ────────────────────

/// Build the transition that LOCKS `collateral` NATIVE (agent → [`VAULT`], mirrored in
/// [`COLLAT`]) and ADVANCES `floor(collateral × ltv_bps / 10_000)` credits with matching
/// [`DEBT`]. NATIVE is conserved (I3) — the two SetBalance net to zero on the supply
/// counter, so the cap chokepoint is untouched.
pub fn borrow_against_collateral(
    state: &SigilState,
    agent: &WalletId,
    collateral: Amount,
    ltv_bps: u32,
    at_height: u64,
) -> Result<StateTransition, LoanError> {
    if ltv_bps > MAX_LTV_BPS {
        return Err(LoanError::LtvTooHigh { bps: ltv_bps, max: MAX_LTV_BPS });
    }
    let borrow = apply_ltv(collateral, ltv_bps).ok_or(LoanError::Overflow)?;

    // Lock collateral: move NATIVE agent → VAULT, mirror into COLLAT.
    let agent_native = Amount::from_ore(state.balance_of(agent, &NATIVE));
    let new_agent_native = agent_native.checked_sub(collateral).ok_or(LoanError::TreasuryDry {
        have: agent_native.as_ore(),
        need: collateral.as_ore(),
    })?;
    let new_vault_native =
        Amount::from_ore(state.balance_of(&VAULT, &NATIVE)).checked_add(collateral).ok_or(LoanError::Overflow)?;
    let new_collat = collateral_of(state, agent).checked_add(collateral).ok_or(LoanError::Overflow)?;

    // Advance credits == borrow, with matching debt (limit == borrow, so it always fits).
    let new_debt = debt_of(state, agent).checked_add(borrow).ok_or(LoanError::Overflow)?;
    let float = credits_of(state, &TREASURY);
    let new_float = float.checked_sub(borrow).ok_or(LoanError::TreasuryDry {
        have: float.as_ore(),
        need: borrow.as_ore(),
    })?;
    let new_agent_credits = credits_of(state, agent).checked_add(borrow).ok_or(LoanError::Overflow)?;

    Ok(StateTransition {
        at_height,
        mutations: vec![
            StateMutation::SetBalance { wallet: *agent, token: NATIVE, amount: new_agent_native.as_ore() },
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: new_vault_native.as_ore() },
            StateMutation::SetBalance { wallet: *agent, token: COLLAT, amount: new_collat.as_ore() },
            StateMutation::SetBalance { wallet: TREASURY, token: CREDITS, amount: new_float.as_ore() },
            StateMutation::SetBalance { wallet: *agent, token: CREDITS, amount: new_agent_credits.as_ore() },
            StateMutation::SetBalance { wallet: *agent, token: DEBT, amount: new_debt.as_ore() },
        ],
    })
}

/// Build the transition that repays `amount`. On FULL payback (debt hits 0) the agent's
/// entire locked collateral is released VAULT → agent. Partial payback only reduces debt.
pub fn repay_and_release(
    state: &SigilState,
    agent: &WalletId,
    amount: Amount,
    at_height: u64,
) -> Result<StateTransition, LoanError> {
    // Reuse the borger repay for the credit half (debt + credit movement + guards).
    let mut t = repay(state, agent, amount, at_height)?;

    let owe = debt_of(state, agent);
    let fully_paid = owe.as_ore() == amount.as_ore();
    if fully_paid {
        let locked = collateral_of(state, agent);
        if locked.as_ore() > 0 {
            let new_vault = Amount::from_ore(state.balance_of(&VAULT, &NATIVE))
                .checked_sub(locked)
                .ok_or(LoanError::Overflow)?;
            let new_agent_native =
                Amount::from_ore(state.balance_of(agent, &NATIVE)).checked_add(locked).ok_or(LoanError::Overflow)?;
            t.mutations.push(StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: new_vault.as_ore() });
            t.mutations.push(StateMutation::SetBalance { wallet: *agent, token: NATIVE, amount: new_agent_native.as_ore() });
            t.mutations.push(StateMutation::SetBalance { wallet: *agent, token: COLLAT, amount: 0 });
        }
    }
    Ok(t)
}

/// Build the transition that LIQUIDATES a defaulted agent loan: seize the locked collateral
/// VAULT → [`TREASURY`] (covering the float lost to the unpaid advance), and zero out the
/// agent's DEBT + COLLAT. Net-sum (I7): the seized collateral lands in the treasury, nothing
/// is minted or destroyed. NATIVE stays conserved.
pub fn liquidate(
    state: &SigilState,
    agent: &WalletId,
    at_height: u64,
) -> Result<StateTransition, LoanError> {
    let locked = collateral_of(state, agent);
    if locked.as_ore() == 0 {
        return Err(LoanError::NoCollateral);
    }
    let new_vault =
        Amount::from_ore(state.balance_of(&VAULT, &NATIVE)).checked_sub(locked).ok_or(LoanError::Overflow)?;
    let new_treasury_native =
        Amount::from_ore(state.balance_of(&TREASURY, &NATIVE)).checked_add(locked).ok_or(LoanError::Overflow)?;
    Ok(StateTransition {
        at_height,
        mutations: vec![
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: new_vault.as_ore() },
            StateMutation::SetBalance { wallet: TREASURY, token: NATIVE, amount: new_treasury_native.as_ore() },
            StateMutation::SetBalance { wallet: *agent, token: COLLAT, amount: 0 },
            StateMutation::SetBalance { wallet: *agent, token: DEBT, amount: 0 },
        ],
    })
}
