//! sigil-usds — USDS (usdSIGIL), SIGIL's native $-pegged stablecoin.
//!
//! Mint: lock `sigil_amount` of NATIVE SIGIL into the [`VAULT`] and receive USDS
//! worth the same USD value at the committed oracle price. Redeem: burn USDS,
//! release the SIGIL at the current price.
//!
//! Invariants:
//! - **NATIVE is conserved.** Minting MOVES SIGIL user→vault (it is never
//!   created or destroyed), so the 21M cap is untouched and the chokepoint's
//!   supply check is a no-op delta.
//! - **USDS supply tracks collateral.** USDS is a separate (uncapped) token;
//!   its balance grows on mint and shrinks on redeem, backed 1:1-by-value by
//!   the SIGIL in the vault at the oracle price.
//! - **Everything is committed in roots** via `commit_state_transition` — no
//!   side ledger (the Quillon-postmortem discipline).
//!
//! Units: SIGIL price is USD×1e8 per SIGIL (`sigil_oracle::PRICE_SCALE`); USDS
//! has 8 decimals ($1 == 1e8 base). So `usds = sigil_amount × price / PRICE_SCALE`.

use sigil_oracle::{read_price, PRICE_SCALE};
use sigil_state::{
    commit_state_transition, CommitError, SigilState, StateMutation, StateTransition, TokenId,
    WalletId, NATIVE,
};

/// USDS token id.
pub const USDS: TokenId = [0xD5; 32];
/// The collateral vault that holds locked SIGIL backing the USDS supply.
pub const VAULT: WalletId = [0x0B; 32];

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
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Mint USDS by locking `sigil_amount` of NATIVE into the vault. Returns the
/// USDS minted to `user`.
pub fn mint(
    state: &mut SigilState,
    height: u64,
    user: WalletId,
    sigil_amount: u128,
) -> Result<u128, UsdsError> {
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
    let usds = sigil_amount
        .checked_mul(price)
        .ok_or(UsdsError::Overflow)?
        / PRICE_SCALE;
    if usds == 0 {
        return Err(UsdsError::ZeroAmount);
    }

    let vault_sigil = state.balance_of(&VAULT, &NATIVE);
    let user_usds = state.balance_of(&user, &USDS);
    let t = StateTransition {
        at_height: height,
        mutations: vec![
            // lock collateral: user → vault (NATIVE conserved)
            StateMutation::SetBalance { wallet: user, token: NATIVE, amount: user_sigil - sigil_amount },
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: vault_sigil + sigil_amount },
            // mint USDS to the user
            StateMutation::SetBalance { wallet: user, token: USDS, amount: user_usds + usds },
        ],
    };
    commit_state_transition(state, &t, height)?;
    Ok(usds)
}

/// Redeem `usds_amount` USDS for the equivalent SIGIL from the vault at the
/// current price. Returns the SIGIL released to `user`.
pub fn redeem(
    state: &mut SigilState,
    height: u64,
    user: WalletId,
    usds_amount: u128,
) -> Result<u128, UsdsError> {
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
    let sigil_out = usds_amount
        .checked_mul(PRICE_SCALE)
        .ok_or(UsdsError::Overflow)?
        / price;
    let vault_sigil = state.balance_of(&VAULT, &NATIVE);
    if vault_sigil < sigil_out {
        return Err(UsdsError::VaultUnderfunded);
    }
    let user_sigil = state.balance_of(&user, &NATIVE);
    let t = StateTransition {
        at_height: height,
        mutations: vec![
            // burn USDS
            StateMutation::SetBalance { wallet: user, token: USDS, amount: user_usds - usds_amount },
            // release collateral: vault → user (NATIVE conserved)
            StateMutation::SetBalance { wallet: VAULT, token: NATIVE, amount: vault_sigil - sigil_out },
            StateMutation::SetBalance { wallet: user, token: NATIVE, amount: user_sigil + sigil_out },
        ],
    };
    commit_state_transition(state, &t, height)?;
    Ok(sigil_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_oracle::{update_price, ORACLE_AUTHORITY};

    const USER: WalletId = [0x11; 32];

    // genesis: fund USER with 10 SIGIL (10 × 1e8 base), set price $2.00.
    fn genesis() -> SigilState {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance { wallet: USER, token: NATIVE, amount: 10 * 100_000_000 }],
        };
        commit_state_transition(&mut s, &t, 0).unwrap();
        update_price(&mut s, 1, ORACLE_AUTHORITY, 2 * PRICE_SCALE).unwrap(); // $2 / SIGIL
        s
    }

    fn native_total(s: &SigilState) -> u128 {
        s.balance_of(&USER, &NATIVE) + s.balance_of(&VAULT, &NATIVE)
    }

    #[test]
    fn mint_pegs_and_conserves_native() {
        let mut s = genesis();
        let before = native_total(&s);
        // lock 3 SIGIL at $2 → $6 of USDS = 6 × 1e8 base
        let usds = mint(&mut s, 2, USER, 3 * 100_000_000).unwrap();
        assert_eq!(usds, 6 * 100_000_000, "3 SIGIL @ $2 = $6 USDS");
        assert_eq!(s.balance_of(&USER, &USDS), 6 * 100_000_000);
        assert_eq!(s.balance_of(&USER, &NATIVE), 7 * 100_000_000, "3 SIGIL locked");
        assert_eq!(s.balance_of(&VAULT, &NATIVE), 3 * 100_000_000, "vault holds collateral");
        // NATIVE conserved (moved, not minted) → 21M cap untouched
        assert_eq!(native_total(&s), before);
    }

    #[test]
    fn redeem_returns_collateral() {
        let mut s = genesis();
        mint(&mut s, 2, USER, 3 * 100_000_000).unwrap(); // 6 USDS, 3 SIGIL locked
        // redeem $6 USDS at $2 → 3 SIGIL back
        let sigil = redeem(&mut s, 3, USER, 6 * 100_000_000).unwrap();
        assert_eq!(sigil, 3 * 100_000_000);
        assert_eq!(s.balance_of(&USER, &USDS), 0, "USDS burned");
        assert_eq!(s.balance_of(&USER, &NATIVE), 10 * 100_000_000, "collateral fully returned");
        assert_eq!(s.balance_of(&VAULT, &NATIVE), 0, "vault emptied");
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
}
