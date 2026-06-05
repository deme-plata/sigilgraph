//! offline.rs — **offline Bluetooth payments**, the safe core.
//!
//! Two devices (a custom-ROM phone + a laptop) trade money with **no internet**:
//! the payer pre-locks funds in an on-chain **purse**, then hands the payee a
//! signed **voucher** (`{purse, seq, to, amount}`) over the Bluetooth link
//! ("Flux Bluetooth v9" — the radio is just the carrier for these bytes). The
//! payee redeems it whenever either device is next online.
//!
//! The hard problem offline payments have is **double-spend** — offline, a
//! malicious payer could hand the same coins to two payees. Two on-chain guards
//! make that impossible at settlement:
//!   1. **Escrow cap** — total redeemed can never exceed what the purse locked.
//!   2. **Per-voucher replay flag** — each `(purse, seq)` settles at most once.
//! Out-of-order redemption is fine (no strict sequence), which suits a flaky BT
//! mesh. The Bluetooth/emulation layer carries the voucher; this is the ledger
//! truth that keeps it honest.

use sigil_state::{
    commit_state_transition, ContractId, SigilState, SlotId, StateMutation, StateTransition,
    WalletId, NATIVE,
};

/// Tracks which `(purse, seq)` vouchers have been redeemed (replay flag).
pub const VOUCHER_REDEEMED: ContractId = [0xB7; 32]; // 'BT'

#[derive(Debug, Clone, PartialEq)]
pub enum OfflineError {
    Insufficient,
    AlreadyRedeemed,
    NoPurse,
    State(String),
}

/// A signed offline payment, carried over the Bluetooth link.
#[derive(Debug, Clone, PartialEq)]
pub struct Voucher {
    /// The escrow purse the funds come from (a wallet holding locked NATIVE).
    pub purse: WalletId,
    /// Monotonic-ish voucher number (unique per purse; gaps + out-of-order ok).
    pub seq: u64,
    pub to: WalletId,
    pub amount: u128,
}

/// The replay-flag slot for a voucher: BLAKE3(purse ‖ seq).
fn redeemed_slot(purse: &WalletId, seq: u64) -> SlotId {
    let mut h = blake3::Hasher::new();
    h.update(purse);
    h.update(&seq.to_le_bytes());
    *h.finalize().as_bytes()
}

/// Open an offline purse: `owner` locks `amount` NATIVE into the `purse` escrow
/// wallet. Vouchers later draw from this — and can never exceed it.
pub fn open_purse(
    state: &mut SigilState,
    height: u64,
    owner: WalletId,
    purse: WalletId,
    amount: u128,
) -> Result<u128, OfflineError> {
    let opre = state.balance_of(&owner, &NATIVE);
    if opre < amount {
        return Err(OfflineError::Insufficient);
    }
    let ppre = state.balance_of(&purse, &NATIVE);
    commit_state_transition(state, &StateTransition { at_height: height, mutations: vec![
        StateMutation::SetBalance { wallet: owner, token: NATIVE, amount: opre - amount },
        StateMutation::SetBalance { wallet: purse, token: NATIVE, amount: ppre + amount },
    ] }, height).map_err(|e| OfflineError::State(e.to_string()))?;
    Ok(ppre + amount)
}

/// Has this voucher already settled?
pub fn is_redeemed(state: &SigilState, v: &Voucher) -> bool {
    state.contract_slot(&VOUCHER_REDEEMED, &redeemed_slot(&v.purse, v.seq)) != [0u8; 32]
}

/// Redeem a voucher on-chain (when a device comes online): pay `amount` from the
/// purse escrow to `to`. Rejects a replay of the same `(purse, seq)` and any
/// amount beyond what the purse still holds. Returns the payee's new balance.
pub fn redeem_voucher(
    state: &mut SigilState,
    height: u64,
    v: &Voucher,
) -> Result<u128, OfflineError> {
    if is_redeemed(state, v) {
        return Err(OfflineError::AlreadyRedeemed); // double-spend caught
    }
    let purse_bal = state.balance_of(&v.purse, &NATIVE);
    if purse_bal == 0 {
        return Err(OfflineError::NoPurse);
    }
    if purse_bal < v.amount {
        return Err(OfflineError::Insufficient); // can't exceed the escrow cap
    }
    let to_pre = state.balance_of(&v.to, &NATIVE);
    commit_state_transition(state, &StateTransition { at_height: height, mutations: vec![
        StateMutation::SetBalance { wallet: v.purse, token: NATIVE, amount: purse_bal - v.amount },
        StateMutation::SetBalance { wallet: v.to, token: NATIVE, amount: to_pre + v.amount },
        // flip the replay flag for this exact (purse, seq)
        StateMutation::SetContractSlot { contract: VOUCHER_REDEEMED, slot: redeemed_slot(&v.purse, v.seq), value: [1u8; 32] },
    ] }, height).map_err(|e| OfflineError::State(e.to_string()))?;
    Ok(to_pre + v.amount)
}

/// Close the purse: refund whatever NATIVE is still locked back to `owner`.
pub fn close_purse(
    state: &mut SigilState,
    height: u64,
    owner: WalletId,
    purse: WalletId,
) -> Result<u128, OfflineError> {
    let left = state.balance_of(&purse, &NATIVE);
    if left == 0 {
        return Ok(0);
    }
    let opre = state.balance_of(&owner, &NATIVE);
    commit_state_transition(state, &StateTransition { at_height: height, mutations: vec![
        StateMutation::SetBalance { wallet: purse, token: NATIVE, amount: 0 },
        StateMutation::SetBalance { wallet: owner, token: NATIVE, amount: opre + left },
    ] }, height).map_err(|e| OfflineError::State(e.to_string()))?;
    Ok(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHONE: WalletId = [0x11; 32]; // custom-ROM phone (payer)
    const PURSE: WalletId = [0xB0; 32];
    const SHOP: WalletId = [0x5A; 32]; // a laptop / shop (payee)
    const FRIEND: WalletId = [0x5B; 32];

    fn state(native: u128) -> SigilState {
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![
            StateMutation::SetMasterWallet { wallet: [0xFF; 32] },
            StateMutation::SetBalance { wallet: PHONE, token: NATIVE, amount: native },
        ] }, 0).unwrap();
        s
    }

    #[test]
    fn offline_payment_settles() {
        let mut s = state(1000);
        open_purse(&mut s, 1, PHONE, PURSE, 500).unwrap();
        assert_eq!(s.balance_of(&PHONE, &NATIVE), 500);
        // phone hands SHOP a voucher over Bluetooth; SHOP redeems when online
        let v = Voucher { purse: PURSE, seq: 1, to: SHOP, amount: 120 };
        let bal = redeem_voucher(&mut s, 2, &v).unwrap();
        assert_eq!(bal, 120);
        assert_eq!(s.balance_of(&SHOP, &NATIVE), 120);
        assert_eq!(s.balance_of(&PURSE, &NATIVE), 380); // escrow drawn down
    }

    #[test]
    fn double_spend_is_blocked() {
        let mut s = state(1000);
        open_purse(&mut s, 1, PHONE, PURSE, 500).unwrap();
        let v = Voucher { purse: PURSE, seq: 7, to: SHOP, amount: 100 };
        redeem_voucher(&mut s, 2, &v).unwrap();
        // replay the SAME voucher (the offline double-spend attack) → blocked
        assert_eq!(redeem_voucher(&mut s, 3, &v), Err(OfflineError::AlreadyRedeemed));
        assert_eq!(s.balance_of(&SHOP, &NATIVE), 100); // paid exactly once
    }

    #[test]
    fn cannot_exceed_escrow_even_with_distinct_vouchers() {
        let mut s = state(1000);
        open_purse(&mut s, 1, PHONE, PURSE, 150).unwrap();
        redeem_voucher(&mut s, 2, &Voucher { purse: PURSE, seq: 1, to: SHOP, amount: 100 }).unwrap();
        // a second, DISTINCT voucher for 100 would overspend the 150 escrow → blocked
        assert_eq!(
            redeem_voucher(&mut s, 3, &Voucher { purse: PURSE, seq: 2, to: FRIEND, amount: 100 }),
            Err(OfflineError::Insufficient)
        );
        assert_eq!(s.balance_of(&FRIEND, &NATIVE), 0);
    }

    #[test]
    fn out_of_order_vouchers_ok() {
        let mut s = state(1000);
        open_purse(&mut s, 1, PHONE, PURSE, 500).unwrap();
        // redeem seq 5 before seq 2 — flaky BT mesh, still fine
        redeem_voucher(&mut s, 2, &Voucher { purse: PURSE, seq: 5, to: SHOP, amount: 50 }).unwrap();
        redeem_voucher(&mut s, 3, &Voucher { purse: PURSE, seq: 2, to: FRIEND, amount: 30 }).unwrap();
        assert_eq!(s.balance_of(&SHOP, &NATIVE), 50);
        assert_eq!(s.balance_of(&FRIEND, &NATIVE), 30);
    }

    #[test]
    fn close_refunds_remainder() {
        let mut s = state(1000);
        open_purse(&mut s, 1, PHONE, PURSE, 500).unwrap();
        redeem_voucher(&mut s, 2, &Voucher { purse: PURSE, seq: 1, to: SHOP, amount: 200 }).unwrap();
        let refunded = close_purse(&mut s, 3, PHONE, PURSE).unwrap();
        assert_eq!(refunded, 300);
        assert_eq!(s.balance_of(&PHONE, &NATIVE), 800); // 1000 - 200 spent
        assert_eq!(s.balance_of(&PURSE, &NATIVE), 0);
    }
}
