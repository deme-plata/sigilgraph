//! sigil-oracle — the committed-in-roots SIGIL price feed.
//!
//! The price (USD per 1 SIGIL, fixed-point — `1e8` = $1.00) lives in a contract
//! storage slot, so it is committed in `contract_state_root` like every other
//! state write — there is NO separate, mutable oracle state to drift or be
//! blindly overwritten (the Quillon-postmortem discipline). Only the pinned
//! [`ORACLE_AUTHORITY`] may update it; every update goes through
//! `commit_state_transition`. USDS reads this price to mint/redeem at peg.
//!
//! ## The sanity band (2026-08-18, direct lesson from a live QUGUSD incident)
//!
//! Quillon's QUGUSD stablecoin reads its collateral price straight from a
//! DEX pool's raw, single-block reserves with no deviation check beyond a
//! generous absolute range — and a separate float→u128 cast bug pinned that
//! pool's reserve near `u128::MAX`, which the price feed happily accepted:
//! observed live at **$43,749.85/SIGIL-equivalent against a ~$3,000 real
//! target**, still unfixed at the time this was written. `read_price` here
//! can never inherit that SPECIFIC bug (there's no float cast, no raw AMM
//! read — a human authority pushes an integer), but "one authority pushes
//! whatever they type" has its own failure mode: a fat-fingered price is
//! exactly as dangerous to USDS's peg as a corrupted AMM read is to
//! QUGUSD's. [`update_price`] therefore rejects any single push that moves
//! the price by more than [`MAX_PRICE_CHANGE_BPS`] from the last COMMITTED
//! price — a real step price change still lands, just over more than one
//! push, giving a chance to notice and abort a bad one before it can be used
//! to mint/redeem USDS at a wrong peg.

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

/// Maximum single-push price move, in basis points of the PREVIOUS
/// committed price (`10_000` = 100%). **`2_000` = 20%** — generous enough
/// that a real, fast-moving market never gets stuck (push again next tick,
/// same direction, and the next 20% clears), tight enough that a single
/// fat-fingered or corrupted push can't blow through the peg in one step.
/// Only applies once a price has ever been set — the FIRST push (from `0`,
/// meaning "never initialized") is unbounded, same as `collateral_vault.rs`'s
/// (dead, in Quillon) circuit breaker intended, except this one is actually
/// enforced on every path, with no bypass.
pub const MAX_PRICE_CHANGE_BPS: u128 = 2_000;

#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    #[error("only the pinned oracle authority may push prices")]
    Unauthorized,
    #[error("price must be > 0")]
    ZeroPrice,
    #[error("price move of {moved_bps} bps exceeds the {MAX_PRICE_CHANGE_BPS} bps sanity band (from {old} to {new})")]
    MoveTooLarge { old: u128, new: u128, moved_bps: u128 },
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Push a new price (USD×1e8 per SIGIL). Authority-gated, sanity-banded (see
/// module docs), committed in `contract_state_root`.
pub fn update_price(
    state: &mut SigilState,
    height: u64,
    feeder: WalletId,
    price: u128,
) -> Result<(), OracleError> {
    if feeder != ORACLE_AUTHORITY {
        return Err(OracleError::Unauthorized);
    }
    if price == 0 {
        return Err(OracleError::ZeroPrice);
    }
    let old = read_price(state);
    if old != 0 {
        // Basis points moved, relative to the OLD price — symmetric for a
        // rise or a fall (both compare the absolute delta against `old`).
        let delta = old.abs_diff(price);
        let moved_bps = delta.saturating_mul(10_000) / old;
        if moved_bps > MAX_PRICE_CHANGE_BPS {
            return Err(OracleError::MoveTooLarge { old, new: price, moved_bps });
        }
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
    fn price_updates_overwrite_within_the_band() {
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        // +15% — inside the 20% band.
        update_price(&mut s, 1, ORACLE_AUTHORITY, 115 * PRICE_SCALE / 100).unwrap();
        assert_eq!(read_price(&s), 115 * PRICE_SCALE / 100);
    }

    #[test]
    fn the_first_push_ever_is_unbounded() {
        // Old price is 0 (never set) — a huge first push must still land;
        // the band only protects an ALREADY-initialized feed.
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, 1_000 * PRICE_SCALE).unwrap();
        assert_eq!(read_price(&s), 1_000 * PRICE_SCALE);
    }

    #[test]
    fn a_push_that_triples_the_price_is_rejected() {
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        let err = update_price(&mut s, 1, ORACLE_AUTHORITY, 3 * PRICE_SCALE).unwrap_err();
        assert!(matches!(err, OracleError::MoveTooLarge { .. }), "a +200% single push must be rejected, got {err:?}");
        assert_eq!(read_price(&s), PRICE_SCALE, "the rejected push must not have landed");
    }

    #[test]
    fn a_push_that_crashes_the_price_is_also_rejected() {
        // The band is symmetric — a crash is exactly as dangerous to a CDP
        // as a spike (it's what makes redemptions look "free").
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        let err = update_price(&mut s, 1, ORACLE_AUTHORITY, PRICE_SCALE / 10).unwrap_err();
        assert!(matches!(err, OracleError::MoveTooLarge { .. }));
    }

    #[test]
    fn a_real_step_change_lands_over_several_pushes() {
        // The band doesn't trap a genuine, sustained market move — it just
        // spreads it over more than one push, which is the whole point.
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        for h in 1..=6 {
            // +15%/push compounds past 2x within the 20%-per-push band.
            let next = read_price(&s) * 115 / 100;
            update_price(&mut s, h, ORACLE_AUTHORITY, next).unwrap();
        }
        assert!(read_price(&s) > 2 * PRICE_SCALE, "a sustained move must eventually clear, got {}", read_price(&s));
    }

    #[test]
    fn zero_price_is_rejected() {
        let mut s = fresh();
        assert!(matches!(update_price(&mut s, 0, ORACLE_AUTHORITY, 0), Err(OracleError::ZeroPrice)));
    }
}
