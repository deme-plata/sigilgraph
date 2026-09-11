//! sigil-oracle — the committed-in-roots SIGIL price feed.
//!
//! The price (USD per 1 SIGIL, fixed-point — `1e8` = $1.00) lives in a contract
//! storage slot, so it is committed in `contract_state_root` like every other
//! state write — there is NO separate, mutable oracle state to drift or be
//! blindly overwritten (the Quillon-postmortem discipline). Every update goes
//! through `commit_state_transition`. USDS reads this price to mint/redeem at peg.
//!
//! ## Who may push (2026-09-11, the "USDS live" change)
//!
//! The genesis [`ORACLE_AUTHORITY`] placeholder (`[0x0A;32]`) has no keyholder,
//! so `sigil-tx` accepts `OraclePush` from the state-committed **master wallet**
//! (Viktor's dev-fee wallet). That made the price feed a thing only the operator
//! could refresh by hand — a stablecoin whose peg waits for a human to tap a
//! phone is not live. So the master may now **delegate**: one master-signed
//! `OracleDelegate` writes the feeder's wallet into [`FEEDER_SLOT`], and from
//! `sigil_usds::USDS_LIVE_HEIGHT` on, `OraclePush` is accepted from the master
//! OR that feeder. The delegation is state-committed (it rides
//! `contract_state_root`), so every node agrees on who the feeder is — never an
//! env var, which is how two nodes come to disagree about validity.
//!
//! ## Freshness (same change)
//!
//! A price that nobody has refreshed is a price that no longer describes the
//! market, and minting USDS against it is minting against a number, not a
//! value. Every push after activation ALSO commits the height it landed at
//! ([`PRICE_HEIGHT_SLOT`]); `sigil-usds` refuses to mint, redeem or pay welfare
//! when the committed price is older than [`MAX_PRICE_AGE_BLOCKS`]. A dead
//! feeder therefore freezes the stablecoin instead of mispricing it — fail
//! closed, the same posture every other money path on this chain takes.
//!
//! Units: `PRICE_SCALE` = USD×1e8 per WHOLE SIGIL. Heights are block heights.

use sigil_state::{
    commit_state_transition, CommitError, ContractId, SigilState, SlotId, StateMutation,
    StateTransition, WalletId,
};

/// The oracle contract's address (storage namespace for the feed).
pub const ORACLE_CONTRACT: ContractId = [0x0C; 32];
/// Slot holding the current SIGIL price.
pub const PRICE_SLOT: SlotId = [0x01; 32];
/// Slot holding the wallet the master has delegated price pushes to
/// (all-zero = nobody delegated). Written only by a master-signed
/// `SigilTx::OracleDelegate`.
pub const FEEDER_SLOT: SlotId = [0x02; 32];
/// Slot holding the block height of the most recent price push (LE u64 in
/// the first 8 bytes; all-zero = never pushed under the height-stamping
/// rule, which `price_is_fresh` treats as STALE on purpose).
pub const PRICE_HEIGHT_SLOT: SlotId = [0x03; 32];
/// The single wallet permitted to push prices (genesis-pinned, DNS-anchorable).
pub const ORACLE_AUTHORITY: WalletId = [0x0A; 32];
/// Fixed-point scale: price is USD×1e8 per 1 SIGIL. `100_000_000` == $1.00.
pub const PRICE_SCALE: u128 = 100_000_000;
/// How old (in blocks) a committed price may be before USDS refuses to use it.
/// 40,000 blocks ≈ 1.7 h at the 6.6 blk/s measured on g2 (2026-09-02). The
/// feeder re-pushes far more often than that (every ~10 min, and on every
/// ≥0.5 % move), so in normal operation this never binds; it binds exactly
/// when the feeder has died, which is when it should.
pub const MAX_PRICE_AGE_BLOCKS: u64 = 40_000;

#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    #[error("only the pinned oracle authority may push prices")]
    Unauthorized,
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Encode a price into a 32-byte slot value — LE u128 in the first 16 bytes.
/// ONE encoder shared by the direct path and `sigil-tx`'s `OraclePush` arm,
/// so `read_price` sees exactly what either wrote.
pub fn encode_price(price: u128) -> [u8; 32] {
    let mut value = [0u8; 32];
    value[..16].copy_from_slice(&price.to_le_bytes());
    value
}

/// Encode a height into a 32-byte slot value — LE u64 in the first 8 bytes.
pub fn encode_height(height: u64) -> [u8; 32] {
    let mut value = [0u8; 32];
    value[..8].copy_from_slice(&height.to_le_bytes());
    value
}

/// The mutations one price push commits under the height-stamping rule:
/// the price itself AND the height it landed at. `sigil-tx` applies exactly
/// these for an `OraclePush` at or after `USDS_LIVE_HEIGHT`; before that it
/// writes only the price slot (the pre-activation shape, byte-identical to
/// what every node already computes for historical pushes).
pub fn price_mutations(price: u128, height: u64) -> [StateMutation; 2] {
    [
        StateMutation::SetContractSlot { contract: ORACLE_CONTRACT, slot: PRICE_SLOT, value: encode_price(price) },
        StateMutation::SetContractSlot { contract: ORACLE_CONTRACT, slot: PRICE_HEIGHT_SLOT, value: encode_height(height) },
    ]
}

/// The mutation a master-signed `OracleDelegate` commits: the feeder's wallet
/// into [`FEEDER_SLOT`]. Delegating to the all-zero wallet REVOKES (nobody
/// but the master may push again).
pub fn delegate_mutation(feeder: WalletId) -> StateMutation {
    StateMutation::SetContractSlot { contract: ORACLE_CONTRACT, slot: FEEDER_SLOT, value: feeder }
}

/// Push a new price (USD×1e8 per SIGIL) directly — the standalone/test path.
/// Authority-gated on the genesis placeholder; committed in
/// `contract_state_root` with the height stamp, exactly like an accepted
/// `OraclePush` after activation.
pub fn update_price(
    state: &mut SigilState,
    height: u64,
    feeder: WalletId,
    price: u128,
) -> Result<(), OracleError> {
    if feeder != ORACLE_AUTHORITY {
        return Err(OracleError::Unauthorized);
    }
    let t = StateTransition { at_height: height, mutations: price_mutations(price, height).to_vec() };
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

/// Height of the most recent height-stamped push. `0` if the price was never
/// pushed under the stamping rule (including a price pushed by a pre-activation
/// binary, which is deliberately treated as stale).
pub fn read_price_height(state: &SigilState) -> u64 {
    let v = state.contract_slot(&ORACLE_CONTRACT, &PRICE_HEIGHT_SLOT);
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[..8]);
    u64::from_le_bytes(b)
}

/// The wallet the master has delegated pushes to, if any.
pub fn read_feeder(state: &SigilState) -> Option<WalletId> {
    let v = state.contract_slot(&ORACLE_CONTRACT, &FEEDER_SLOT);
    if v == [0u8; 32] { None } else { Some(v) }
}

/// Is the committed price usable at `at_height`? Requires a non-zero price,
/// a height stamp, and `at_height - stamp <= MAX_PRICE_AGE_BLOCKS`. A stamp
/// in the FUTURE (only possible on a reorg/replay edge) is also refused —
/// a price from a block that has not happened is not a price.
pub fn price_is_fresh(state: &SigilState, at_height: u64) -> bool {
    let stamp = read_price_height(state);
    read_price(state) != 0
        && stamp != 0
        && stamp <= at_height
        && at_height - stamp <= MAX_PRICE_AGE_BLOCKS
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
        assert_eq!(read_price_height(&fresh()), 0);
        assert_eq!(read_feeder(&fresh()), None);
    }

    #[test]
    fn price_updates_overwrite() {
        let mut s = fresh();
        update_price(&mut s, 0, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        update_price(&mut s, 1, ORACLE_AUTHORITY, 3 * PRICE_SCALE).unwrap();
        assert_eq!(read_price(&s), 3 * PRICE_SCALE);
        assert_eq!(read_price_height(&s), 1, "the stamp follows the latest push");
    }

    #[test]
    fn freshness_is_a_window_after_the_stamp() {
        let mut s = fresh();
        assert!(!price_is_fresh(&s, 10), "no price at all is stale");
        update_price(&mut s, 100, ORACLE_AUTHORITY, PRICE_SCALE).unwrap();
        assert!(price_is_fresh(&s, 100), "same block as the push");
        assert!(price_is_fresh(&s, 100 + MAX_PRICE_AGE_BLOCKS), "exactly at the edge is still fresh");
        assert!(!price_is_fresh(&s, 101 + MAX_PRICE_AGE_BLOCKS), "one past the edge is stale");
        assert!(!price_is_fresh(&s, 99), "a stamp in the future is refused");
    }

    #[test]
    fn a_price_slot_without_a_height_stamp_is_stale() {
        // The pre-activation shape: only PRICE_SLOT written. Must read as
        // stale so a mint cannot key off a price whose age is unknown.
        let mut s = fresh();
        let t = StateTransition {
            at_height: 5,
            mutations: vec![StateMutation::SetContractSlot {
                contract: ORACLE_CONTRACT, slot: PRICE_SLOT, value: encode_price(PRICE_SCALE),
            }],
        };
        commit_state_transition(&mut s, &t, 5).unwrap();
        assert_eq!(read_price(&s), PRICE_SCALE);
        assert!(!price_is_fresh(&s, 5));
    }

    #[test]
    fn delegation_round_trips_and_zero_revokes() {
        let mut s = fresh();
        let feeder: WalletId = [0xFE; 32];
        let t = StateTransition { at_height: 1, mutations: vec![delegate_mutation(feeder)] };
        commit_state_transition(&mut s, &t, 1).unwrap();
        assert_eq!(read_feeder(&s), Some(feeder));
        let t = StateTransition { at_height: 2, mutations: vec![delegate_mutation([0u8; 32])] };
        commit_state_transition(&mut s, &t, 2).unwrap();
        assert_eq!(read_feeder(&s), None, "all-zero revokes");
    }

    #[test]
    fn encoders_are_le_prefixes() {
        assert_eq!(&encode_price(PRICE_SCALE)[..16], &PRICE_SCALE.to_le_bytes());
        assert_eq!(&encode_height(6_400_000)[..8], &6_400_000u64.to_le_bytes());
    }
}
