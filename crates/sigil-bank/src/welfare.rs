//! welfare.rs — **SIGIL-Nation welfare policy** (v7.5 line, whitepaper v1.5 era).
//!
//! The operator's vision: citizens of the SIGIL nation receive a periodic
//! welfare stipend, financed by a slice of the protocol's mining dev fee.
//! (The QUG / QUGUSD leg of welfare financing lives on Quillon Graph and is
//! operational, not consensus — this module is the SIGIL-side, on-chain leg.)
//!
//! ## The money route, end to end
//!
//! 1. Every block reward is already split by [`crate::split_mining_reward`]:
//!    500 bps dev fee → master, 120 bps → commons, 10 bps → operator pool.
//! 2. From [`WELFARE_FROM_HEIGHT`], [`split_mining_reward_at`] raises the
//!    gross dev fee to [`MASTER_MINING_FEE_BPS_V2`] (750 bps, operator
//!    directive 2026-08-31) and routes [`WELFARE_MINING_FEE_BPS`] (200 bps)
//!    of it into [`WELFARE_WALLET`] — master nets 550 bps, welfare is
//!    financed by the dev fee as promised, and the 250 bps raise comes out
//!    of the validator share (deliberate, documented at the constant).
//! 3. An attested citizen (borger registry, `sigil_tx::CitizenAttest`)
//!    claims [`WELFARE_STIPEND_GLYPHS`] at most once per
//!    [`WELFARE_CLAIM_INTERVAL_BLOCKS`] via `sigil_tx::WelfareClaim`.
//!    Claims are refused while the treasury can't cover the stipend —
//!    the system is self-limiting, never minting.
//!
//! Pure math + constants only, same discipline as the rest of sigil-bank:
//! the chokepoint (`sigil_state::commit_state_transition`) stays the single
//! thing that touches storage.

use crate::{BankError, MiningSplit, WalletId, BPS_DENOMINATOR};

/// The SIGIL-Nation welfare treasury wallet. Receives the welfare carve of
/// every post-activation coinbase; debited only by `WelfareClaim`
/// transactions. `0x57` = ASCII `'W'`.
pub const WELFARE_WALLET: WalletId = [0x57; 32];

/// Contract id of the welfare claim ledger: slot = citizen wallet, value =
/// big-endian last-claim height in the first 8 bytes ([`encode_claim_height`]).
/// `0x77` = ASCII `'w'`.
pub const WELFARE_LEDGER: [u8; 32] = [0x77; 32];

/// Contract id of the borger.dk-style citizen registry (wallet → cpr_hash
/// attestation). Moved here from `sigil-rpc::nation` so the consensus layer
/// (`sigil-tx`) can check citizenship without a dependency cycle;
/// `sigil-rpc` re-exports it, so existing callers keep compiling.
pub const BORGER_REGISTRY: [u8; 32] = [0x0B; 32];

/// Welfare slice of the mining reward, in basis points, carved out of the
/// post-activation dev fee ([`MASTER_MINING_FEE_BPS_V2`]): 200 of 750.
pub const WELFARE_MINING_FEE_BPS: u128 = 200;

/// Post-activation gross dev fee, in basis points: **750 bps = 7.5%**,
/// raised from the genesis 500 bps by operator directive (Viktor,
/// 2026-08-31: *"raise my dev fee to 7-8% … with 40% of the dev fee to
/// this system I want a little more"* — 7.5% is the midpoint of the asked
/// range). Net to the master after the welfare carve: 750 − 200 =
/// **550 bps (5.5%)**, a little more than the original 5%. Applies ONLY
/// from [`WELFARE_FROM_HEIGHT`]; every block below it splits at the
/// genesis [`crate::MASTER_MINING_FEE_BPS`] (500), so historical replay is
/// byte-identical. Unlike the first welfare design, this DOES lower the
/// miner's take post-activation (by 250 bps) — that is the point of the
/// raise, stated here so nobody reads it as an accident.
pub const MASTER_MINING_FEE_BPS_V2: u128 = 750;

/// Activation height for the whole nation-welfare feature: the coinbase
/// carve, `CitizenAttest`, and `WelfareClaim` are all refused below it.
/// Chosen ~1.5–4 days past height 526k (2026-08-31, ~1–2.7 blk/s) so the
/// v7.5.0 client release rolls to auto-updating installs first — a block
/// containing an unknown tx variant is undecodable by old clients, so the
/// gate doubles as the decoder-skew grace window.
pub const WELFARE_FROM_HEIGHT: u64 = 900_000;

/// Minimum blocks between two claims by the same citizen. ~200k blocks is
/// roughly a day at the measured 2.7 blk/s live rate (≈20 h; ≈67 h at the
/// 0.83 blk/s lull rate) — "daily-ish", deliberately in block time not wall
/// time so replay is deterministic.
pub const WELFARE_CLAIM_INTERVAL_BLOCKS: u64 = 200_000;

/// Stipend per claim, in glyphs (10-decimal base units): 1.0 SIGIL.
/// Sized against measured emission: ~2% of block rewards flows in, so the
/// treasury sustains on the order of tens of daily claimants today and
/// grows with emission; an empty treasury refuses claims rather than mints.
pub const WELFARE_STIPEND_GLYPHS: u128 = 10_000_000_000;

/// The stipend in USDS base units ($1 == 1e8): **$1.00 per claim interval.**
/// Operator ruling 2026-08-31 ("paid out in sigil sUSD for certainty and no
/// volatility"): the payout asset is USDS, so the stipend is a promise about
/// purchasing power, not a bet on SIGIL's price. `WelfareClaim` locks
/// treasury SIGIL into the USDS vault as collateral and mints exactly this
/// amount to the citizen (`sigil_usds::plan_welfare_mint`). The GLYPHS
/// constant above now serves only as the claim-fee ceiling.
pub const WELFARE_STIPEND_USD_E8: u128 = 100_000_000;

/// Is the nation-welfare feature active at `height`?
pub fn welfare_active(height: u64) -> bool {
    height >= WELFARE_FROM_HEIGHT
}

/// Height-aware mining split: identical to [`crate::split_mining_reward`]
/// below [`WELFARE_FROM_HEIGHT`]; from activation, the gross dev fee rises
/// to [`MASTER_MINING_FEE_BPS_V2`] (750 bps) and [`WELFARE_MINING_FEE_BPS`]
/// (200 bps) of it flows to the welfare treasury — master nets 550 bps.
///
/// Invariants (tested):
/// - `operator_share` and `commons_share` are unchanged by activation.
/// - The validator take drops by exactly the fee raise (250 bps) at
///   activation — deliberate, see [`MASTER_MINING_FEE_BPS_V2`].
/// - Exact conservation: the four shares plus `welfare_share` always sum to
///   `reward`.
pub fn split_mining_reward_at(
    reward: u128,
    master_wallet: Option<WalletId>,
    height: u64,
) -> Result<MiningSplit, BankError> {
    let mut split = crate::split_mining_reward(reward, master_wallet)?;
    if master_wallet.is_none() || reward == 0 || !welfare_active(height) {
        return Ok(split);
    }
    // Post-activation gross dev fee (750 bps), replacing the legacy 500.
    let master_gross = reward
        .checked_mul(MASTER_MINING_FEE_BPS_V2)
        .ok_or(BankError::MathOverflow)?
        / BPS_DENOMINATOR;
    let welfare_share = reward
        .checked_mul(WELFARE_MINING_FEE_BPS)
        .ok_or(BankError::MathOverflow)?
        / BPS_DENOMINATOR;
    // Fail LOUD, never silently. These are money splits: if the dev/operator/
    // commons fee bps were ever misconfigured to exceed 100% of the reward, a raw
    // subtraction would underflow u128 into a colossal validator_share — minting
    // supply from nowhere. checked_sub turns that into a clean MathOverflow the
    // coinbase path rejects, in release too (unlike the old debug-only assert).
    // Today the taken shares sum to 880 bps (750 + 10 + 120) and 750 ≥ 200, so
    // neither of these ever fires — this is a guard against a future bps change.
    split.validator_share = reward
        .checked_sub(master_gross)
        .and_then(|v| v.checked_sub(split.operator_share))
        .and_then(|v| v.checked_sub(split.commons_share))
        .ok_or(BankError::MathOverflow)?;
    split.master_share = master_gross
        .checked_sub(welfare_share)
        .ok_or(BankError::MathOverflow)?;
    split.welfare_share = welfare_share;
    Ok(split)
}

/// Encode a claim height into a 32-byte contract-slot value (big-endian u64
/// in the first 8 bytes, rest zero).
pub fn encode_claim_height(height: u64) -> [u8; 32] {
    let mut v = [0u8; 32];
    v[..8].copy_from_slice(&height.to_be_bytes());
    v
}

/// Decode a claim height from a contract-slot value. The all-zero slot (no
/// claim ever) decodes to 0, which is always claim-eligible.
pub fn decode_claim_height(value: &[u8; 32]) -> u64 {
    u64::from_be_bytes(value[..8].try_into().expect("8 bytes"))
}

/// Is a citizen whose last claim was at `last_claim` eligible again at
/// `height`? A never-claimed citizen has `last_claim == 0` and is eligible
/// as soon as the feature is active.
pub fn claim_eligible(last_claim: u64, height: u64) -> bool {
    welfare_active(height)
        && (last_claim == 0 || height >= last_claim.saturating_add(WELFARE_CLAIM_INTERVAL_BLOCKS))
}

/// The first height at which the next claim is allowed, given the last one.
pub fn next_claim_height(last_claim: u64) -> u64 {
    if last_claim == 0 {
        WELFARE_FROM_HEIGHT
    } else {
        last_claim.saturating_add(WELFARE_CLAIM_INTERVAL_BLOCKS).max(WELFARE_FROM_HEIGHT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split_mining_reward;

    const MASTER: Option<WalletId> = Some([1u8; 32]);

    #[test]
    fn split_identical_below_activation() {
        let legacy = split_mining_reward(1_000_000, MASTER).unwrap();
        let at = split_mining_reward_at(1_000_000, MASTER, WELFARE_FROM_HEIGHT - 1).unwrap();
        assert_eq!(legacy, at, "pre-activation blocks must replay byte-identically");
        assert_eq!(at.welfare_share, 0);
    }

    #[test]
    fn split_raises_dev_fee_and_carves_welfare_after_activation() {
        let reward = 1_000_000u128;
        let legacy = split_mining_reward(reward, MASTER).unwrap();
        let at = split_mining_reward_at(reward, MASTER, WELFARE_FROM_HEIGHT).unwrap();
        // Operator pool and commons untouched by activation.
        assert_eq!(at.operator_share, legacy.operator_share);
        assert_eq!(at.commons_share, legacy.commons_share);
        // Gross dev fee 750 bps: master nets 550, welfare gets 200.
        assert_eq!(at.welfare_share, reward * WELFARE_MINING_FEE_BPS / BPS_DENOMINATOR);
        assert_eq!(
            at.master_share,
            reward * (MASTER_MINING_FEE_BPS_V2 - WELFARE_MINING_FEE_BPS) / BPS_DENOMINATOR
        );
        // The validator absorbs exactly the 250 bps raise.
        assert_eq!(
            legacy.validator_share - at.validator_share,
            reward * (MASTER_MINING_FEE_BPS_V2 - crate::MASTER_MINING_FEE_BPS) / BPS_DENOMINATOR
        );
        // Exact conservation over all five shares.
        assert_eq!(
            at.validator_share + at.master_share + at.operator_share + at.commons_share + at.welfare_share,
            reward
        );
    }

    #[test]
    fn split_no_master_means_no_welfare() {
        let at = split_mining_reward_at(1_000_000, None, WELFARE_FROM_HEIGHT).unwrap();
        assert_eq!(at.validator_share, 1_000_000);
        assert_eq!(at.welfare_share, 0);
    }

    #[test]
    fn odd_amounts_conserve_exactly() {
        // Floor-division rounding must never create or destroy a glyph.
        for reward in [1u128, 7, 99, 199, 10_001, 123_456_789, u64::MAX as u128] {
            let at = split_mining_reward_at(reward, MASTER, WELFARE_FROM_HEIGHT).unwrap();
            assert_eq!(
                at.validator_share + at.master_share + at.operator_share + at.commons_share + at.welfare_share,
                reward,
                "conservation failed at reward={reward}"
            );
        }
    }

    #[test]
    fn claim_height_roundtrip() {
        for h in [0u64, 1, 900_000, u64::MAX] {
            assert_eq!(decode_claim_height(&encode_claim_height(h)), h);
        }
    }

    #[test]
    fn claim_eligibility_windows() {
        // Inactive before the feature height, even for a never-claimer.
        assert!(!claim_eligible(0, WELFARE_FROM_HEIGHT - 1));
        // Never claimed → eligible at activation.
        assert!(claim_eligible(0, WELFARE_FROM_HEIGHT));
        // Just claimed → locked out for the interval.
        let h = WELFARE_FROM_HEIGHT + 5;
        assert!(!claim_eligible(h, h));
        assert!(!claim_eligible(h, h + WELFARE_CLAIM_INTERVAL_BLOCKS - 1));
        assert!(claim_eligible(h, h + WELFARE_CLAIM_INTERVAL_BLOCKS));
        assert_eq!(next_claim_height(h), h + WELFARE_CLAIM_INTERVAL_BLOCKS);
        assert_eq!(next_claim_height(0), WELFARE_FROM_HEIGHT);
    }
}
