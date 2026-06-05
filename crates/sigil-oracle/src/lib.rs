//! sigil-oracle — the committed-in-roots SIGIL price feed.
//!
//! The price (USD per 1 SIGIL, fixed-point — `1e8` = $1.00) lives in a contract
//! storage slot, so it is committed in `contract_state_root` like every other
//! state write — there is NO separate, mutable oracle state to drift or be
//! blindly overwritten (the Quillon-postmortem discipline). Only the pinned
//! [`ORACLE_AUTHORITY`] may update it; every update goes through
//! `commit_state_transition`. USDS reads this price to mint/redeem at peg.

use sigil_state::{
    commit_state_transition, CommitError, ContractId, SigilState, SlotId, StateMutation,
    StateTransition, WalletId,
};

/// The oracle contract's address (storage namespace for the feed).
pub const ORACLE_CONTRACT: ContractId = [0x0C; 32];
/// Slot holding the current SIGIL price.
pub const PRICE_SLOT: SlotId = [0x01; 32];
/// The single wallet permitted to push prices (genesis-pinned, DNS-anchorable).
pub const ORACLE_AUTHORITY: WalletId = [0x0A; 32];
/// Fixed-point scale: price is USD×1e8 per 1 SIGIL. `100_000_000` == $1.00.
pub const PRICE_SCALE: u128 = 100_000_000;

#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    #[error("only the pinned oracle authority may push prices")]
    Unauthorized,
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Push a new price (USD×1e8 per SIGIL). Authority-gated; committed in
/// `contract_state_root`.
pub fn update_price(
    state: &mut SigilState,
    height: u64,
    feeder: WalletId,
    price: u128,
) -> Result<(), OracleError> {
    if feeder != ORACLE_AUTHORITY {
        return Err(OracleError::Unauthorized);
    }
    let mut value = [0u8; 32];
    value[..16].copy_from_slice(&price.to_le_bytes());
    let t = StateTransition {
        at_height: height,
        mutations: vec![StateMutation::SetContractSlot {
            contract: ORACLE_CONTRACT,
            slot: PRICE_SLOT,
            value,
        }],
    };
    commit_state_transition(state, &t, height)?;
    Ok(())
}

/// The committed price (USD×1e8 per SIGIL). `0` if never set.
pub fn read_price(state: &SigilState) -> u128 {
    let v = state.contract_slot(&ORACLE_CONTRACT, &PRICE_SLOT);
    let mut b = [0u8; 16];
    b.copy_from_slice(&v[..16]);
    u128::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> SigilState {
        SigilState::new()
    }

    #[test]
    fn authority_can_push_and_read_committed() {
        let mut s = fresh();
        let roots_before = s.roots();
        // $2.50 per SIGIL → 2.5 × 1e8
        update_price(&mut s, 0, ORACLE_AUTHORITY, 250_000_000).unwrap();
        assert_eq!(read_price(&s), 250_000_000);
        // committed → contract_state_root changed
        assert_ne!(s.roots().contract_state_root, roots_before.contract_state_root);
    }

    #[test]
    fn non_authority_rejected() {
        let mut s = fresh();
        let imposter: WalletId = [0x99; 32];
        let r = update_price(&mut s, 0, imposter, 100_000_000);
        assert!(matches!(r, Err(OracleError::Unauthorized)));
        assert_eq!(read_price(&s), 0, "no price written by imposter");
    }

    #[test]
    fn unset_price_is_zero() {
        assert_eq!(read_price(&fresh()), 0);
    }

    #[test]
    fn price_updates_overwrite() {
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        update_price(&mut s, 1, ORACLE_AUTHORITY, 3 * PRICE_SCALE).unwrap();
        assert_eq!(read_price(&s), 3 * PRICE_SCALE);
    }
}
