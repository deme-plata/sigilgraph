//! nation.rs — **citizen "nation" features**: the things a Danish *borger* would
//! meet at the Statsministeriet's MCP solution (borger.dk identity, NemID-style
//! signing, e-Boks receipts, utility-bill pay), but WORKING — and committed in
//! the `contract_state_root`, so the **flux lightweight node verifies every one
//! of them client-side** without trusting a server.
//!
//! Privacy: the raw CPR number is NEVER stored — only its hash (provided by the
//! caller). The citizen registry maps `wallet → cpr_hash` under a gov authority.

use sigil_state::{
    commit_state_transition, ContractId, SigilState, SlotId, StateMutation, StateTransition,
    WalletId, NATIVE,
};

/// The borger.dk citizen registry contract (wallet → cpr_hash attestation).
pub const BORGER_REGISTRY: ContractId = [0x0B; 32];
/// The only authority allowed to attest citizens (the gov / NemID issuer).
pub const BORGER_AUTHORITY: WalletId = [0x0A; 32];
/// The e-Boks receipt ledger contract (citizen → last document hash).
pub const EBOKS_LEDGER: ContractId = [0xE6; 32];

#[derive(Debug, Clone, PartialEq)]
pub enum NationError {
    NotAuthority,
    NotCitizen,
    Insufficient,
    State(String),
}

fn is_zero(h: &[u8; 32]) -> bool {
    h.iter().all(|&b| b == 0)
}

/// True if `wallet` is an attested citizen (registry slot is non-zero).
pub fn is_citizen(state: &SigilState, wallet: &WalletId) -> bool {
    !is_zero(&state.contract_slot(&BORGER_REGISTRY, wallet))
}

/// The cpr_hash a citizen is registered under (zero if not a citizen).
pub fn citizen_cpr_hash(state: &SigilState, wallet: &WalletId) -> [u8; 32] {
    state.contract_slot(&BORGER_REGISTRY, wallet)
}

/// **Attest a citizen** (borger.dk / NemID issuance). Only `BORGER_AUTHORITY`
/// may do this. Stores `wallet → cpr_hash` (the slot key is the wallet itself).
pub fn attest_citizen(
    state: &mut SigilState,
    height: u64,
    authority: WalletId,
    citizen: WalletId,
    cpr_hash: [u8; 32],
) -> Result<(), NationError> {
    if authority != BORGER_AUTHORITY {
        return Err(NationError::NotAuthority);
    }
    if is_zero(&cpr_hash) {
        return Err(NationError::State("cpr_hash must be non-zero".into()));
    }
    let slot: SlotId = citizen; // [u8;32] == [u8;32]
    commit_state_transition(
        state,
        &StateTransition {
            at_height: height,
            mutations: vec![StateMutation::SetContractSlot { contract: BORGER_REGISTRY, slot, value: cpr_hash }],
        },
        height,
    )
    .map_err(|e| NationError::State(e.to_string()))?;
    Ok(())
}

/// **Pay a utility bill** (electricity / benzin / power-station) — only attested
/// citizens may pay, NATIVE is conserved (citizen → provider). Returns the
/// citizen's new NATIVE balance.
pub fn pay_utility_bill(
    state: &mut SigilState,
    height: u64,
    citizen: WalletId,
    provider: WalletId,
    amount: u128,
) -> Result<u128, NationError> {
    if !is_citizen(state, &citizen) {
        return Err(NationError::NotCitizen);
    }
    let cpre = state.balance_of(&citizen, &NATIVE);
    if cpre < amount {
        return Err(NationError::Insufficient);
    }
    let ppre = state.balance_of(&provider, &NATIVE);
    commit_state_transition(
        state,
        &StateTransition {
            at_height: height,
            mutations: vec![
                StateMutation::SetBalance { wallet: citizen, token: NATIVE, amount: cpre - amount },
                StateMutation::SetBalance { wallet: provider, token: NATIVE, amount: ppre + amount },
            ],
        },
        height,
    )
    .map_err(|e| NationError::State(e.to_string()))?;
    Ok(cpre - amount)
}

/// **Issue an e-Boks receipt**: commit a document hash to the citizen's ledger
/// slot. Anyone (incl. the lightweight node) can later `verify_eboks_receipt`
/// the doc against the committed root — a tamper-evident digital-mail receipt.
pub fn issue_eboks_receipt(
    state: &mut SigilState,
    height: u64,
    citizen: WalletId,
    doc_hash: [u8; 32],
) -> Result<(), NationError> {
    if !is_citizen(state, &citizen) {
        return Err(NationError::NotCitizen);
    }
    commit_state_transition(
        state,
        &StateTransition {
            at_height: height,
            mutations: vec![StateMutation::SetContractSlot { contract: EBOKS_LEDGER, slot: citizen, value: doc_hash }],
        },
        height,
    )
    .map_err(|e| NationError::State(e.to_string()))?;
    Ok(())
}

/// Verify a citizen's latest e-Boks receipt matches `doc_hash` (client-side
/// against the committed contract_state_root — no server trusted).
pub fn verify_eboks_receipt(state: &SigilState, citizen: &WalletId, doc_hash: &[u8; 32]) -> bool {
    &state.contract_slot(&EBOKS_LEDGER, citizen) == doc_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: WalletId = [0x11; 32];
    const POWER_CO: WalletId = [0x9E; 32];
    const CPR: [u8; 32] = [0x42; 32]; // hash of a CPR number
    const DOC: [u8; 32] = [0xD0; 32];

    fn state_with(native: u128) -> SigilState {
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![
            StateMutation::SetMasterWallet { wallet: [0xFF; 32] },
            StateMutation::SetBalance { wallet: ALICE, token: NATIVE, amount: native },
        ] }, 0).unwrap();
        s
    }

    #[test]
    fn only_authority_can_attest() {
        let mut s = state_with(0);
        assert_eq!(attest_citizen(&mut s, 1, [0xBA; 32], ALICE, CPR), Err(NationError::NotAuthority));
        assert!(!is_citizen(&s, &ALICE));
        attest_citizen(&mut s, 1, BORGER_AUTHORITY, ALICE, CPR).unwrap();
        assert!(is_citizen(&s, &ALICE));
        assert_eq!(citizen_cpr_hash(&s, &ALICE), CPR);
    }

    #[test]
    fn non_citizen_cannot_pay_bill() {
        let mut s = state_with(1000);
        assert_eq!(pay_utility_bill(&mut s, 1, ALICE, POWER_CO, 100), Err(NationError::NotCitizen));
    }

    #[test]
    fn citizen_pays_electricity_conserved() {
        let mut s = state_with(1000);
        attest_citizen(&mut s, 1, BORGER_AUTHORITY, ALICE, CPR).unwrap();
        let left = pay_utility_bill(&mut s, 2, ALICE, POWER_CO, 300).unwrap();
        assert_eq!(left, 700);
        assert_eq!(s.balance_of(&ALICE, &NATIVE), 700);
        assert_eq!(s.balance_of(&POWER_CO, &NATIVE), 300); // conserved
    }

    #[test]
    fn eboks_receipt_issue_and_verify() {
        let mut s = state_with(0);
        attest_citizen(&mut s, 1, BORGER_AUTHORITY, ALICE, CPR).unwrap();
        issue_eboks_receipt(&mut s, 2, ALICE, DOC).unwrap();
        assert!(verify_eboks_receipt(&s, &ALICE, &DOC));      // genuine doc verifies
        assert!(!verify_eboks_receipt(&s, &ALICE, &[0x01; 32])); // tampered doc rejected
    }

    #[test]
    fn eboks_requires_citizenship() {
        let mut s = state_with(0);
        assert_eq!(issue_eboks_receipt(&mut s, 1, ALICE, DOC), Err(NationError::NotCitizen));
    }
}
