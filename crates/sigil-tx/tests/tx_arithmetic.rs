//! sigil-tx arithmetic + balance-safety integration tests (Inv 5/6 source level).
//!
//! `apply_tx` is the single chokepoint every value-moving tx passes through, so
//! its arithmetic is consensus-critical money code. The inline `#[cfg(test)]`
//! module focuses on signature verification; this file pins the *value* guards
//! that a Quillon-style foot-gun (silent wrap / mint-on-alias / spend-more-than-
//! held) would breach:
//!
//!   - a Send that would overflow `amount + fee` aborts LOUDLY (`Overflow`), it
//!     never wraps a balance back below the cap (the documented Quillon fix);
//!   - spending more than held is rejected (`InsufficientBalance`), no block;
//!   - a non-native transfer requires the *token* balance, not just the fee;
//!   - the happy path conserves value exactly: recipient gains `amount`, sender
//!     loses `amount + fee`, the fee is the only thing that leaves the two
//!     wallets (it is debited, credited nowhere at this layer);
//!   - a self-transfer debits ONLY the fee and mints nothing (the aliasing
//!     anti-mint guard, tested here directly on `apply_tx` as defence-in-depth
//!     under the chronos-level `self_send_does_not_mint`).
//!
//! Pure functions over an in-memory `SigilState` — deterministic, instant.

use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes};
use sigil_state::{
    commit_state_transition, SigilState, StateMutation, StateTransition, TokenId, WalletId, NATIVE,
};
use sigil_tx::{apply_tx, batch_into_transition, SigilTx, SignedTx, TxApplyError};

const A: WalletId = [1u8; 32];
const B: WalletId = [2u8; 32];
const TOK: TokenId = [7u8; 32]; // a non-native token

/// Minimal precheck-valid signed tx (mirrors the cross-crate `sign_dummy`):
/// correct-length zero sig for SqiSign5, fee payer == signer, never verified.
fn signed(tx: SigilTx) -> SignedTx {
    let from = tx.fee_payer();
    let len = SigScheme::SqiSign5.expected_sig_len();
    SignedTx {
        tx,
        from_pubkey: from,
        nonce: 0,
        sig_scheme: SigScheme::SqiSign5,
        sig: SignatureBytes(vec![0u8; len]),
        pubkey: PubKeyBytes(Vec::new()),
    }
}

/// Build a state seeded with the given balances by committing absolute
/// `SetBalance`s through the real chokepoint (the only public seeding path).
fn state_with(balances: &[(WalletId, TokenId, u128)]) -> SigilState {
    let mut s = SigilState::new();
    let mutations = balances
        .iter()
        .map(|&(wallet, token, amount)| StateMutation::SetBalance { wallet, token, amount })
        .collect();
    let t = StateTransition { at_height: 1, mutations };
    commit_state_transition(&mut s, &t, 1).expect("seed commit");
    s
}

/// Commit an `apply_tx` result so balances can be read back.
fn apply_and_commit(state: &mut SigilState, tx: SigilTx, at_height: u64) {
    let res = apply_tx(state, &signed(tx)).expect("tx must apply");
    let t = batch_into_transition([res], at_height);
    commit_state_transition(state, &t, at_height).expect("commit");
}

#[test]
fn send_amount_plus_fee_overflow_aborts_loudly() {
    // Sender holds enough to pass the fee precheck, but amount + fee overflows
    // u128. SIGIL must return Overflow — never wrap the addition and let a
    // wrapped-small `need` slip past the balance check (the Quillon foot-gun).
    let s = state_with(&[(A, NATIVE, 10)]);
    let err = apply_tx(
        &s,
        &signed(SigilTx::Send { from: A, to: B, amount: u128::MAX, token: NATIVE, fee: 1 }),
    )
    .unwrap_err();
    assert!(matches!(err, TxApplyError::Overflow), "got {err:?}, expected Overflow");
}

#[test]
fn send_more_than_held_is_rejected() {
    let s = state_with(&[(A, NATIVE, 100)]);
    let err = apply_tx(
        &s,
        &signed(SigilTx::Send { from: A, to: B, amount: 200, token: NATIVE, fee: 1 }),
    )
    .unwrap_err();
    match err {
        TxApplyError::InsufficientBalance { have, need } => {
            assert_eq!(have, 100);
            assert_eq!(need, 201, "need must be amount + fee");
        }
        other => panic!("expected InsufficientBalance, got {other:?}"),
    }
}

#[test]
fn non_native_send_requires_token_balance_not_just_fee() {
    // Sender can pay the native fee but holds zero of the transfer token.
    let s = state_with(&[(A, NATIVE, 10), (A, TOK, 0)]);
    let err = apply_tx(
        &s,
        &signed(SigilTx::Send { from: A, to: B, amount: 5, token: TOK, fee: 1 }),
    )
    .unwrap_err();
    match err {
        TxApplyError::InsufficientBalance { have, need } => {
            assert_eq!((have, need), (0, 5), "must fail on the TOKEN balance, not native");
        }
        other => panic!("expected InsufficientBalance on token, got {other:?}"),
    }
}

#[test]
fn happy_send_conserves_value_minus_fee() {
    let mut s = state_with(&[(A, NATIVE, 1_000)]);
    apply_and_commit(&mut s, SigilTx::Send { from: A, to: B, amount: 100, token: NATIVE, fee: 5 }, 2);
    assert_eq!(s.balance_of(&A, &NATIVE), 895, "sender loses amount + fee");
    assert_eq!(s.balance_of(&B, &NATIVE), 100, "recipient gains exactly amount");
    // The only value to leave the two wallets is the fee (debited, credited
    // nowhere at this layer) — total dropped by exactly 5, never rose.
    let total: u128 = s.balance_of(&A, &NATIVE) + s.balance_of(&B, &NATIVE);
    assert_eq!(total, 995, "no inflation; exactly the fee left the wallet set");
}

#[test]
fn self_send_debits_only_fee_and_mints_nothing() {
    // The aliasing trap: naive debit-then-credit on the same (A, NATIVE) slot
    // would leave balance = before + amount. The guard collapses it to −fee.
    let mut s = state_with(&[(A, NATIVE, 1_000)]);
    apply_and_commit(&mut s, SigilTx::Send { from: A, to: A, amount: 500, token: NATIVE, fee: 5 }, 2);
    assert_eq!(
        s.balance_of(&A, &NATIVE),
        995,
        "self-send must debit only the fee — minting 500 here would be free money"
    );
}

#[test]
fn zero_fee_self_send_is_a_perfect_noop_on_balance() {
    let mut s = state_with(&[(A, NATIVE, 777)]);
    apply_and_commit(&mut s, SigilTx::Send { from: A, to: A, amount: 777, token: NATIVE, fee: 0 }, 2);
    assert_eq!(s.balance_of(&A, &NATIVE), 777, "fee-0 self-send must not change the balance at all");
}
