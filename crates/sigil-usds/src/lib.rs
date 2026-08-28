//! sigil-usds — USDS (usdSIGIL), SIGIL's native $-pegged stablecoin.
//!
//! Mint: lock `sigil_amount` of NATIVE SIGIL into the [`VAULT`] and receive
//! USDS worth SLIGHTLY LESS than the locked USD value — the gap is
//! [`MINT_BUFFER_BPS`], a collateral cushion, not a fee (the fee is separate,
//! see below). Redeem: burn USDS, release the SIGIL at the current price.
//!
//! ## Why a buffer (2026-08-18 design decision)
//!
//! The original version of this module minted USDS worth EXACTLY the locked
//! value (1:1, no cushion) — safe from any leverage/liquidation bug class,
//! but with one real gap: if SIGIL's price falls between one user's mint and
//! a LATER user's redemption, the vault can come up short (the same failure
//! mode QUGUSD defends against with a 135% collateral ratio — see
//! `MANDAT_QUILLON_BANK_KREDIT.md` era research). Rather than copy QUGUSD's
//! full CDP + liquidation machinery (more code, more risk surface, and two
//! independent mint paths is exactly the bug class that hurt QUGUSD — see
//! the 2026-08-18 design review), USDS keeps its single, simple mint/redeem
//! path and adds a modest FIXED cushion instead: every mint locks
//! [`MINT_BUFFER_BPS`] (105%) of the USD value it issues, so the vault
//! always holds a same-day margin against a price move, without any
//! per-position liquidation logic to get wrong.
//!
//! ## Why a fee (same design decision)
//!
//! Every mint and redeem also pays [`sigil_bank::MASTER_SWAP_FEE_BPS`] (the
//! SAME protocol fee already charged on DEX swaps, reused rather than
//! inventing a new constant) via [`sigil_bank::split_swap_output`] — the
//! master wallet earns a small, consistent cut of stablecoin activity, same
//! as it does on trading activity. This is deliberately the SAME function
//! swaps already use, not a parallel implementation, so a future change to
//! the protocol fee rate updates both paths at once.
//!
//! Invariants:
//! - **NATIVE is conserved.** Minting MOVES SIGIL user→vault (it is never
//!   created or destroyed), so the 21M cap is untouched and the chokepoint's
//!   supply check is a no-op delta. The redeem-side fee moves a sliver of
//!   that same NATIVE from vault→master wallet — still conserved, just
//!   reassigned.
//! - **USDS supply tracks collateral, with a margin.** USDS is a separate
//!   (uncapped) token; its balance grows on mint and shrinks on redeem,
//!   backed by AT LEAST its face value in vault SIGIL at issue time (105% of
//!   it, before any price movement).
//! - **Everything is committed in roots** via `commit_state_transition` — no
//!   side ledger (the Quillon-postmortem discipline).
//!
//! Units: SIGIL price is USD×1e8 per SIGIL (`sigil_oracle::PRICE_SCALE`); USDS
//! has 8 decimals ($1 == 1e8 base).

use sigil_bank::{split_swap_output, BankError, DEV_MASTER_WALLET};
use sigil_oracle::{read_price, PRICE_SCALE};
use sigil_state::{
    commit_state_transition, CommitError, SigilState, StateMutation, StateTransition, TokenId,
    WalletId, NATIVE,
};

/// USDS token id.
pub const USDS: TokenId = [0xD5; 32];
/// The collateral vault that holds locked SIGIL backing the USDS supply.
pub const VAULT: WalletId = [0x0B; 32];

/// Collateral buffer over 1:1 value, in the same basis-point convention
/// `sigil_bank` uses (`10_000` = 100%). **`10_500` = 105%**: locking $X of
/// SIGIL mints `$X / 1.05` of USDS, leaving the remaining ~4.76% of the
/// locked value sitting in the vault as a cushion against SIGIL's price
/// falling before a later redemption. Fixed at genesis, like every other
/// protocol-rate constant in this codebase — a future consensus upgrade can
/// change it.
pub const MINT_BUFFER_BPS: u128 = 10_500;

#[derive(Debug, thiserror::Error)]
pub enum UsdsError {
    #[error("oracle price is unset (0) — cannot mint/redeem")]
    NoPrice,
    #[error("amount is zero")]
    ZeroAmount,
    #[error("insufficient SIGIL collateral")]
    InsufficientSigil,
    #[error("insufficient USDS balance")]
    InsufficientUsds,
    #[error("vault underfunded for this redemption")]
    VaultUnderfunded,
    #[error("arithmetic overflow")]
    Overflow,
    #[error("protocol fee split: {0}")]
    Fee(#[from] BankError),
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Pure outcome of [`plan_mint`] — the mutations it would take, and how much
/// USDS the user would actually receive, WITHOUT having committed anything.
#[derive(Debug, Clone)]
pub struct MintPlan {
    /// USDS that would be credited to the user (after buffer + fee).
    pub usds_to_user: u128,
    /// The exact mutations [`mint`] (or a caller integrating USDS into a
    /// larger batched transition, e.g. `sigil-tx::apply_tx`) must commit.
    pub mutations: Vec<StateMutation>,
}

/// Plan a mint: lock `sigil_amount` of NATIVE into the vault, work out how
/// much USDS that yields after the buffer + protocol fee — but do NOT touch
/// storage. Read-only (`&SigilState`), so this is safe to call from inside
/// `sigil-tx::apply_tx`'s immutable-state pass, exactly like `sigil_dex::swap`
/// already is for `SigilTx::Swap`. [`mint`] is a thin wrapper that plans then
/// commits, for direct/standalone callers (tests, tools) that aren't routing
/// through a `SigilTx`.
pub fn plan_mint(state: &SigilState, user: WalletId, sigil_amount: u128) -> Result<MintPlan, UsdsError> {
    if sigil_amount == 0 {
        return Err(UsdsError::ZeroAmount);
    }
    let price = read_price(state);
    if price == 0 {
        return Err(UsdsError::NoPrice);
    }
    let user_sigil = state.balance_of(&user, &NATIVE);
    if user_sigil < sigil_amount {
        return Err(UsdsError::InsufficientSigil);
    }

    let locked_value = sigil_amount.checked_mul(price).ok_or(UsdsError::Overflow)? / PRICE_SCALE;
    // The buffer: mint only `value / 1.05` in USDS — see module docs.
    let usds_gross = locked_value
        .checked_mul(sigil_bank::BPS_DENOMINATOR)
        .ok_or(UsdsError::Overflow)?
        / MINT_BUFFER_BPS;
    if usds_gross == 0 {
        return Err(UsdsError::ZeroAmount);
    }
    // The protocol fee: same split every DEX swap already pays.
    let split = split_swap_output(usds_gross, Some(DEV_MASTER_WALLET))?;

    let vault_sigil = state.balance_of(&VAULT, &NATIVE);
    let user_usds = state.balance_of(&user, &USDS);
    let master_usds = state.balance_of(&DEV_MASTER_WALLET, &USDS);
    Ok(MintPlan {
        usds_to_user: split.user_share,
        mutations: vec![
            // lock collateral: user → vault (NATIVE conserved), FULL amount —
            // the buffer is what's NOT minted back out as USDS, not a bigger lock.
            StateMutation::SetBalance { wallet: user, token: NATIVE, amount: user_sigil - sigil_amount },
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: vault_sigil + sigil_amount },
            // mint USDS: user's share, plus the protocol's fee share.
            StateMutation::SetBalance { wallet: user, token: USDS, amount: user_usds + split.user_share },
            StateMutation::SetBalance {
                wallet: DEV_MASTER_WALLET,
                token: USDS,
                amount: master_usds + split.master_share,
            },
        ],
    })
}

/// Mint USDS by locking `sigil_amount` of NATIVE into the vault, committing
/// immediately. See [`plan_mint`] for the pure version this wraps — use that
/// instead when integrating into a larger batched transition (e.g. a
/// `SigilTx`). Returns the USDS actually credited to `user`.
pub fn mint(
    state: &mut SigilState,
    height: u64,
    user: WalletId,
    sigil_amount: u128,
) -> Result<u128, UsdsError> {
    let plan = plan_mint(state, user, sigil_amount)?;
    let t = StateTransition { at_height: height, mutations: plan.mutations };
    commit_state_transition(state, &t, height)?;
    Ok(plan.usds_to_user)
}

/// Pure outcome of [`plan_redeem`] — mirrors [`MintPlan`].
#[derive(Debug, Clone)]
pub struct RedeemPlan {
    /// SIGIL that would be credited to the user (after the protocol fee).
    pub sigil_to_user: u128,
    /// The exact mutations [`redeem`] (or a `SigilTx`-integrating caller)
    /// must commit.
    pub mutations: Vec<StateMutation>,
}

/// Plan a redemption: burn `usds_amount` USDS, work out the SIGIL released
/// from the vault at the current price minus the protocol fee — read-only,
/// same "plan then commit" split as [`plan_mint`]/[`mint`].
pub fn plan_redeem(state: &SigilState, user: WalletId, usds_amount: u128) -> Result<RedeemPlan, UsdsError> {
    if usds_amount == 0 {
        return Err(UsdsError::ZeroAmount);
    }
    let price = read_price(state);
    if price == 0 {
        return Err(UsdsError::NoPrice);
    }
    let user_usds = state.balance_of(&user, &USDS);
    if user_usds < usds_amount {
        return Err(UsdsError::InsufficientUsds);
    }
    let sigil_gross = usds_amount.checked_mul(PRICE_SCALE).ok_or(UsdsError::Overflow)? / price;
    let split = split_swap_output(sigil_gross, Some(DEV_MASTER_WALLET))?;

    let vault_sigil = state.balance_of(&VAULT, &NATIVE);
    if vault_sigil < sigil_gross {
        return Err(UsdsError::VaultUnderfunded);
    }
    let user_sigil = state.balance_of(&user, &NATIVE);
    let master_sigil = state.balance_of(&DEV_MASTER_WALLET, &NATIVE);
    Ok(RedeemPlan {
        sigil_to_user: split.user_share,
        mutations: vec![
            // burn USDS
            StateMutation::SetBalance { wallet: user, token: USDS, amount: user_usds - usds_amount },
            // release collateral: vault → user's share + master's fee share
            // (NATIVE conserved throughout — just reassigned).
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: vault_sigil - sigil_gross },
            StateMutation::SetBalance { wallet: user, token: NATIVE, amount: user_sigil + split.user_share },
            StateMutation::SetBalance {
                wallet: DEV_MASTER_WALLET,
                token: NATIVE,
                amount: master_sigil + split.master_share,
            },
        ],
    })
}

/// Redeem `usds_amount` USDS for SIGIL from the vault, committing
/// immediately. See [`plan_redeem`] for the pure version. Returns the SIGIL
/// actually credited to `user`.
pub fn redeem(
    state: &mut SigilState,
    height: u64,
    user: WalletId,
    usds_amount: u128,
) -> Result<u128, UsdsError> {
    let plan = plan_redeem(state, user, usds_amount)?;
    let t = StateTransition {
        at_height: height,
        mutations: plan.mutations,
    };
    commit_state_transition(state, &t, height)?;
    Ok(plan.sigil_to_user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oracle::{update_price, ORACLE_AUTHORITY};

    const USER: WalletId = [0x11; 32];

    // genesis: fund USER with 21 SIGIL (21 × 1e8 base), set price $1.00 —
    // chosen so 21 × 10000/10500 lands exactly on 20e8 (no rounding noise
    // in the buffer math, so fee math is the only rounding in play).
    fn genesis() -> SigilState {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance { wallet: USER, token: NATIVE, amount: 21 * 100_000_000 }],
        };
        commit_state_transition(&mut s, &t, 0).unwrap();
        update_price(&mut s, 1, ORACLE_AUTHORITY, PRICE_SCALE).unwrap(); // $1 / SIGIL
        s
    }

    fn native_total(s: &SigilState) -> u128 {
        s.balance_of(&USER, &NATIVE) + s.balance_of(&VAULT, &NATIVE) + s.balance_of(&DEV_MASTER_WALLET, &NATIVE)
    }

    #[test]
    fn mint_applies_the_buffer_before_the_fee() {
        let mut s = genesis();
        // lock 21 SIGIL @ $1 = $21 value → buffer'd gross = 21e8 × 10000/10500 = 20e8 ($20 exactly)
        let usds = mint(&mut s, 2, USER, 21 * 100_000_000).unwrap();
        let expected_split = split_swap_output(20 * 100_000_000, Some(DEV_MASTER_WALLET)).unwrap();
        assert_eq!(usds, expected_split.user_share, "user gets the post-fee share of the buffer'd amount");
        assert!(usds < 21 * 100_000_000, "buffer + fee must issue LESS USDS than the raw locked value");
        assert_eq!(s.balance_of(&USER, &USDS), usds);
        assert_eq!(s.balance_of(&DEV_MASTER_WALLET, &USDS), expected_split.master_share, "protocol fee credited in USDS");
        assert_eq!(s.balance_of(&USER, &NATIVE), 0, "the FULL 21 SIGIL is locked, not just the buffer'd portion");
        assert_eq!(s.balance_of(&VAULT, &NATIVE), 21 * 100_000_000, "vault holds the full lock");
    }

    #[test]
    fn vault_holds_more_value_than_outstanding_usds_right_after_mint() {
        // The whole point of the buffer: immediately after a mint, at the
        // SAME price, the vault's collateral value must exceed total USDS
        // supply (the safety margin QUGUSD gets from a 135% ratio; USDS gets
        // it from this fixed buffer instead).
        let mut s = genesis();
        mint(&mut s, 2, USER, 21 * 100_000_000).unwrap();
        let price = read_price(&s);
        let vault_value = s.balance_of(&VAULT, &NATIVE) * price / PRICE_SCALE;
        let total_usds = s.balance_of(&USER, &USDS) + s.balance_of(&DEV_MASTER_WALLET, &USDS);
        assert!(vault_value > total_usds, "vault_value={vault_value} must exceed total_usds={total_usds}");
    }

    #[test]
    fn redeem_pays_the_fee_from_the_released_sigil() {
        let mut s = genesis();
        let usds = mint(&mut s, 2, USER, 21 * 100_000_000).unwrap();
        let before = native_total(&s);

        let sigil_back = redeem(&mut s, 3, USER, usds).unwrap();
        let expected_gross = usds.checked_mul(PRICE_SCALE).unwrap() / read_price(&s);
        let expected_split = split_swap_output(expected_gross, Some(DEV_MASTER_WALLET)).unwrap();
        assert_eq!(sigil_back, expected_split.user_share);
        assert_eq!(s.balance_of(&USER, &USDS), 0, "USDS burned");
        assert!(s.balance_of(&DEV_MASTER_WALLET, &NATIVE) > 0, "protocol earned a SIGIL fee on redemption too");
        // NATIVE is conserved throughout (locked, then reassigned on redeem) —
        // never minted or destroyed.
        assert_eq!(native_total(&s), before);
    }

    #[test]
    fn mint_without_price_fails() {
        let mut s = SigilState::new();
        let t = StateTransition { at_height: 0, mutations: vec![StateMutation::SetBalance { wallet: USER, token: NATIVE, amount: 100_000_000 }] };
        commit_state_transition(&mut s, &t, 0).unwrap();
        assert!(matches!(mint(&mut s, 1, USER, 100_000_000), Err(UsdsError::NoPrice)));
    }

    #[test]
    fn cannot_mint_more_than_collateral() {
        let mut s = genesis();
        assert!(matches!(mint(&mut s, 2, USER, 999 * 100_000_000), Err(UsdsError::InsufficientSigil)));
    }

    #[test]
    fn cannot_redeem_more_than_balance() {
        let mut s = genesis();
        mint(&mut s, 2, USER, 21 * 100_000_000).unwrap();
        let over = s.balance_of(&USER, &USDS) + 1;
        assert!(matches!(redeem(&mut s, 3, USER, over), Err(UsdsError::InsufficientUsds)));
    }
}
