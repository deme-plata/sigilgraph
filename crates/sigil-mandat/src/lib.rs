//! sigil-mandat — MandatPilot's credit ledger, ON SigilGraph but INVISIBLE to users.
//!
//! The chain is the engine room, never the showroom. Users see "credits, kroner, $2,
//! MitID, verificeret" — never coin/wallet/blockchain. Three things make the chain
//! disappear:
//!   1. The account is DERIVED FROM THE MitID IDENTITY ([`account_from_mitid`]) — no
//!      seed phrase, no wallet to connect, no keys to lose. The identity IS the account.
//!   2. Credits are a plain SIGIL token balance; we expose them as the [`Amount`] money
//!      type (flux-uint) so over-/under-flow are impossible by construction.
//!   3. Every top-up / debit is a typed `StateMutation` committed through sigil-state's
//!      chokepoint → committed in roots → `.proof`-auditable. The user never sees that;
//!      auditors and compliance do (sold as trust, not crypto).
//!
//! Credits are PREPAID and SERVICE-ONLY (not a withdrawable bank): if the chain ever
//! hiccups, balances are reconstructable from the Stripe ledger + chain state. Low risk,
//! full dogfood.

use flux_uint::Amount;
use sigil_state::{
    commit_state_transition, SigilState, StateMutation, StateRoots, StateTransition, TokenId,
    WalletId,
};

pub mod onramp;
pub mod verify;
pub mod monitor;
pub use onramp::{apply_payment, credits_for, Payment, CENTS_PER_CREDIT};
pub use verify::{verify_business, CvrRecord, MitidClaims, VerifyResult, VERIFY_COST};
pub use monitor::{diff, monitor_check, watch_start, Change, CvrSnapshot, MONITOR_COST, WATCH_COST};

/// The MandatPilot credits token (distinct from NATIVE = [0;32]). 1 credit = 1 unit.
pub const CREDITS: TokenId = *b"mandat-credits-v1-token-00000001";
/// Where spent credits flow — MandatPilot revenue. Spent credits are conserved, not burned.
pub const TREASURY: WalletId = *b"mandatpilot-treasury-wallet-0001";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MandatError {
    #[error("insufficient credits: have {have}, need {need}")]
    Insufficient { have: u128, need: u128 },
    #[error("credit balance overflow")]
    Overflow,
}

/// THE invisible-chain key: the account is the MitID identity, not a wallet.
/// No seed, no keys — `WalletId = BLAKE3("mandat:acct:" ++ mitid_sub)`.
pub fn account_from_mitid(mitid_sub: &str) -> WalletId {
    let mut h = blake3::Hasher::new();
    h.update(b"mandat:acct:");
    h.update(mitid_sub.as_bytes());
    *h.finalize().as_bytes()
}

/// Current credits as an [`Amount`] (money-safe view over the u128 balance).
pub fn credits_of(state: &SigilState, account: &WalletId) -> Amount {
    Amount::from_ore(state.balance_of(account, &CREDITS))
}

/// Build the transition that TOPS UP an account's credits (after a confirmed Stripe
/// payment). Pure — the caller (Stripe webhook) dedupes the event id for idempotency,
/// so a webhook replay never double-credits. Returns the typed transition to commit.
pub fn topup_credit(
    state: &SigilState,
    account: &WalletId,
    amount: Amount,
    at_height: u64,
) -> Result<StateTransition, MandatError> {
    let cur = credits_of(state, account);
    let new = cur.checked_add(amount).ok_or(MandatError::Overflow)?;
    Ok(StateTransition {
        at_height,
        mutations: vec![StateMutation::SetBalance {
            wallet: *account,
            token: CREDITS,
            amount: new.as_ore(),
        }],
    })
}

/// Build the transition that DEBITS `cost` credits for one product action and routes
/// them to the treasury (conserved — credits move, never vanish). The overdraw guard:
/// returns `Insufficient` (and emits nothing) when the balance can't cover it.
pub fn debit_action(
    state: &SigilState,
    account: &WalletId,
    cost: Amount,
    at_height: u64,
) -> Result<StateTransition, MandatError> {
    let have = credits_of(state, account);
    let new_acct = have.checked_sub(cost).ok_or(MandatError::Insufficient {
        have: have.as_ore(),
        need: cost.as_ore(),
    })?;
    let treasury = credits_of(state, &TREASURY);
    let new_treasury = treasury.checked_add(cost).ok_or(MandatError::Overflow)?;
    Ok(StateTransition {
        at_height,
        mutations: vec![
            StateMutation::SetBalance { wallet: *account, token: CREDITS, amount: new_acct.as_ore() },
            StateMutation::SetBalance { wallet: TREASURY, token: CREDITS, amount: new_treasury.as_ore() },
        ],
    })
}

/// Commit a mandat transition through the chokepoint (writes balances, updates roots).
pub fn commit(state: &mut SigilState, t: &StateTransition, height: u64) -> Result<StateRoots, sigil_state::CommitError> {
    commit_state_transition(state, t, height)
}
