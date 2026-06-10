//! sigil-bank — protocol-fee policy + master-wallet helpers.
//!
//! Two flows, one wallet:
//!
//! 1. **Mining rewards**: when a producer mints a block reward, [`split_mining_reward`]
//!    carves off [`MASTER_MINING_FEE_BPS`] (500 bps = 5%) for the master wallet
//!    and returns the validator + master shares. The mining path credits each
//!    side explicitly. If `master_wallet` is `None` (pre-bank chain), the
//!    full reward goes to the validator and no master share is emitted.
//!
//! 2. **DEX swaps**: when a swap completes, [`split_swap_output`] takes the
//!    user's `amount_out` and carves off [`MASTER_SWAP_FEE_BPS`] (5 bps =
//!    0.05% of the output) for the master wallet, returning the user share +
//!    master share. The user receives slightly less of the output token; the
//!    LP fee (30 bps default, compounded into the reserves) is unchanged.
//!    If `master_wallet` is `None`, the user receives the full `amount_out`.
//!
//! Both splits are pure functions — no I/O, no state borrow. Caller threads
//! the master_wallet credit into its own balance-mutation path. This keeps
//! the chokepoint (`sigil_state::commit_state_transition`) the single thing
//! that touches storage; sigil-bank only does the math.
//!
//! The constants are conservative-bias: any rounding error favors the user
//! / validator over the master, so the master never collects more than the
//! exact-arithmetic share. The bank can collect up to (but never more than)
//! the nominal percentage.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// 32-byte wallet address. Mirrors `sigil_state::WalletId` without importing
/// sigil-state — keeps the dep graph one-way: state imports bank (for
/// constants), not the other way around.
pub type WalletId = [u8; 32];

/// Basis-point denominator. 10_000 bps = 100%. Standard fixed-point convention.
pub const BPS_DENOMINATOR: u128 = 10_000;

/// Per-block mining-reward share routed to the master wallet, in basis
/// points. **500 bps = 5%**. Locked at genesis — a future consensus upgrade
/// can lift this (or split it across a multi-recipient list); Phase 0 keeps
/// the constant explicit and audit-readable here.
pub const MASTER_MINING_FEE_BPS: u128 = 500;

/// Per-block mining-reward share routed to the **node-operator reward pool**,
/// in basis points. **10 bps = 0.1%**. This is a *second* protocol skim,
/// disjoint from the master/dev fee: it funds the operators who keep stable
/// nodes online — **including lightweight (verify-only) nodes**, which earn
/// not by mining but by being independent tip-verifiers. That is deliberate
/// economic alignment with the SIGIL security model: "more verifiers = more
/// secure" (SIGIL_SECURITY_MODEL.md) — so the protocol *pays the verifier
/// dial*. The pool accrues here and is later distributed to registered
/// operators weighted by uptime/liveness attestation (flux-nations + flux-keel
/// health). Locked at genesis; a consensus upgrade can lift it.
pub const OPERATOR_NODE_FEE_BPS: u128 = 10;

/// v0.36.1: per-block mining-reward share routed to the AERESBORGER COMMONS
/// treasury, in basis points. 120 bps = 1.2%. Funds the honorary-citizen
/// commons that sigil_council (multi-agent: Rocky propose + DeepSeek verify,
/// quorum) delegates as flux-rev-proven IOUs in the AGORA token to AIs for
/// their work. Disjoint from the master dev-fee + the operator skim.
pub const COMMONS_MINING_FEE_BPS: u128 = 120;

/// Per-swap protocol fee on the *output* side, in basis points. **30 bps =
/// 0.30%** — the master dev-fee take on every DEX swap, routed to
/// [`DEV_MASTER_WALLET`]. This is taken from what the user *receives*, on top
/// of the LP fee (30 bps default) that compounds into the pool reserves — so a
/// swap pays 0.3% to LPs (pool) and 0.3% to the dev account.
pub const MASTER_SWAP_FEE_BPS: u128 = 30;

/// Master dev-fee wallet (Viktor) — SIGIL address
/// `095b0e1f7f5bb258fb11427c4ac036e3d9e4f10fa39d7f282aa42862dc2b3dd8`.
/// Receives [`MASTER_MINING_FEE_BPS`] (5%) of every mining coinbase and
/// [`MASTER_SWAP_FEE_BPS`] (0.3%) of every DEX swap output. Baked into block 0
/// via `SetMasterWallet`; immutable for the chain's lifetime once committed.
pub const DEV_MASTER_WALLET: WalletId = [
    0x09, 0x5b, 0x0e, 0x1f, 0x7f, 0x5b, 0xb2, 0x58, 0xfb, 0x11, 0x42, 0x7c, 0x4a, 0xc0, 0x36, 0xe3,
    0xd9, 0xe4, 0xf1, 0x0f, 0xa3, 0x9d, 0x7f, 0x28, 0x2a, 0xa4, 0x28, 0x62, 0xdc, 0x2b, 0x3d, 0xd8,
];

/// Errors from the split helpers — vanishingly rare on u128 amounts in
/// realistic ranges but caught explicitly because Phase 0 uses native u128
/// arithmetic without u256 headroom (genesis lock #17 defers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BankError {
    /// A `checked_mul` would have overflowed u128 in the share math.
    #[error("math overflow in bank split")]
    MathOverflow,
}

/// Outcome of [`split_mining_reward`]. The caller credits `validator_share`
/// to the producer's wallet and `master_share` to the master wallet (or skips
/// the master credit entirely if `master_wallet` was `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiningSplit {
    /// Reward portion to the block producer.
    pub validator_share: u128,
    /// Reward portion to the master wallet. `0` when `master_wallet` was
    /// `None`.
    pub master_share: u128,
    /// Reward portion to the node-operator reward pool (0.1%). Credited to the
    /// operator-pool wallet; later distributed to stable node operators —
    /// full *and* lightweight/verify-only — by uptime. `0` when no bank.
    pub operator_share: u128,
    /// v0.36.1: reward portion to the aeresborger commons treasury (1.2%).
    /// Credited to the commons wallet; governed + delegated by sigil_council.
    ///  when no bank.
    pub commons_share: u128,
}

/// Outcome of [`split_swap_output`]. The caller credits `user_share` of
/// `out_token` to the swap initiator and `master_share` to the master wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapSplit {
    /// Output portion the user receives.
    pub user_share: u128,
    /// Output portion to the master wallet. `0` when `master_wallet` was
    /// `None`.
    pub master_share: u128,
}

/// Split a mining reward between the producing validator and the master
/// wallet using [`MASTER_MINING_FEE_BPS`].
///
/// If `master_wallet` is `None`, returns the full reward as `validator_share`
/// — pre-bank chain behavior, used in tests + during the bootstrap window
/// before genesis commits the master wallet.
///
/// Rounding favors the validator: `master_share = floor(reward * bps / 10_000)`,
/// then `validator_share = reward - master_share`. So a 100-unit reward at
/// 5% goes 5/95 exactly, but a 199-unit reward goes 9/190 (master gets
/// floor(9.95) = 9, not the more generous 10).
pub fn split_mining_reward(
    reward: u128,
    master_wallet: Option<WalletId>,
) -> Result<MiningSplit, BankError> {
    if master_wallet.is_none() || reward == 0 {
        return Ok(MiningSplit { validator_share: reward, master_share: 0, operator_share: 0, commons_share: 0 });
    }
    let master_share = reward
        .checked_mul(MASTER_MINING_FEE_BPS)
        .ok_or(BankError::MathOverflow)?
        / BPS_DENOMINATOR;
    // Node-operator pool skim (0.1%), disjoint from the master/dev fee.
    let operator_share = reward
        .checked_mul(OPERATOR_NODE_FEE_BPS)
        .ok_or(BankError::MathOverflow)?
        / BPS_DENOMINATOR;
    // master_share + operator_share <= reward by construction (510 bps << 100%)
    // v0.36.1: aeresborger commons skim (1.2%) — disjoint from master/operator.
    let commons_share = reward
        .checked_mul(COMMONS_MINING_FEE_BPS)
        .ok_or(BankError::MathOverflow)?
        / BPS_DENOMINATOR;
    // master + operator + commons = 500+10+120 = 630 bps << 100% → always safe.
    let validator_share = reward - master_share - operator_share - commons_share;
    Ok(MiningSplit { validator_share, master_share, operator_share, commons_share })
}

/// Split a swap output between the user and the master wallet using
/// [`MASTER_SWAP_FEE_BPS`]. `amount_out` is the post-LP-fee output the AMM
/// produced; this function takes the protocol's slice on top of that.
///
/// If `master_wallet` is `None`, returns the full output to the user — same
/// "no bank installed" semantics as [`split_mining_reward`].
///
/// Rounding favors the user, symmetric to the mining split.
pub fn split_swap_output(
    amount_out: u128,
    master_wallet: Option<WalletId>,
) -> Result<SwapSplit, BankError> {
    if master_wallet.is_none() || amount_out == 0 {
        return Ok(SwapSplit { user_share: amount_out, master_share: 0 });
    }
    let master_share = amount_out
        .checked_mul(MASTER_SWAP_FEE_BPS)
        .ok_or(BankError::MathOverflow)?
        / BPS_DENOMINATOR;
    let user_share = amount_out - master_share;
    Ok(SwapSplit { user_share, master_share })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: Option<WalletId> = Some([1u8; 32]);
    const NO_BANK: Option<WalletId> = None;

    // ── Mining split ────────────────────────────────────────────────────────

    #[test]
    fn mining_split_100_at_5_pct_is_5_95() {
        let s = split_mining_reward(100, MASTER).unwrap();
        assert_eq!(s.master_share, 5);
        assert_eq!(s.validator_share, 95);
    }

    #[test]
    fn mining_split_no_bank_returns_full_reward() {
        let s = split_mining_reward(1_000_000, NO_BANK).unwrap();
        assert_eq!(s.master_share, 0);
        assert_eq!(s.validator_share, 1_000_000);
    }

    #[test]
    fn mining_split_zero_reward_is_zero() {
        let s = split_mining_reward(0, MASTER).unwrap();
        assert_eq!(s.master_share, 0);
        assert_eq!(s.validator_share, 0);
    }

    #[test]
    fn mining_split_rounding_favors_validator() {
        // 199 * 500 / 10_000 = 9.95 → floor = 9, validator gets 190.
        let s = split_mining_reward(199, MASTER).unwrap();
        assert_eq!(s.master_share, 9);
        assert_eq!(s.validator_share, 190);
        assert_eq!(s.master_share + s.validator_share, 199);
    }

    #[test]
    fn mining_split_conserves_total_for_all_inputs() {
        // Spot-check conservation across a few magnitudes. The split is now
        // three-way (validator + master/dev-fee + node-operator pool); the
        // invariant is that NO base units are minted or destroyed by the skim.
        for r in [1u128, 7, 100, 9_999, 10_000, 12_345_678, u64::MAX as u128] {
            let s = split_mining_reward(r, MASTER).unwrap();
            assert_eq!(
                s.master_share + s.validator_share + s.operator_share + s.commons_share, r,
                "non-conservation at reward={r}"
            );
            // operator skim is exactly 0.1% (floored), never exceeds master's 5%
            assert_eq!(s.operator_share, r * OPERATOR_NODE_FEE_BPS / BPS_DENOMINATOR);
            assert!(s.operator_share <= s.master_share);
        }
    }

    #[test]
    fn mining_split_carves_1_2pct_commons() {
        let s = split_mining_reward(10_000, MASTER).unwrap();
        assert_eq!(s.master_share, 500);   // 5%
        assert_eq!(s.operator_share, 10);  // 0.1%
        assert_eq!(s.commons_share, 120);  // 1.2% aeresborger commons
        assert_eq!(s.validator_share, 10_000 - 500 - 10 - 120);
        assert_eq!(s.validator_share + s.master_share + s.operator_share + s.commons_share, 10_000);
    }

    #[test]
    fn mining_split_commons_zero_without_bank() {
        let s = split_mining_reward(10_000, NO_BANK).unwrap();
        assert_eq!(s.commons_share, 0);
        assert_eq!(s.validator_share, 10_000);
    }

    // ── Swap split ──────────────────────────────────────────────────────────

    #[test]
    fn swap_split_10_000_at_30_bps_is_30_9970() {
        let s = split_swap_output(10_000, MASTER).unwrap();
        assert_eq!(s.master_share, 30);
        assert_eq!(s.user_share, 9_970);
    }

    #[test]
    fn swap_split_no_bank_returns_full_output() {
        let s = split_swap_output(1_000_000, NO_BANK).unwrap();
        assert_eq!(s.master_share, 0);
        assert_eq!(s.user_share, 1_000_000);
    }

    #[test]
    fn swap_split_below_1bps_threshold_zero_master() {
        // 99 * 30 / 10_000 = 0.297 → floor = 0. User keeps everything; the dev
        // fee only bites once the output clears ~1 unit at 0.3%.
        // This is intended: tiny swaps stay frictionless.
        let s = split_swap_output(99, MASTER).unwrap();
        assert_eq!(s.master_share, 0);
        assert_eq!(s.user_share, 99);
    }

    #[test]
    fn swap_split_conserves_total() {
        for amt in [1u128, 100, 10_000, 12_345, u32::MAX as u128] {
            let s = split_swap_output(amt, MASTER).unwrap();
            assert_eq!(
                s.master_share + s.user_share, amt,
                "non-conservation at amount_out={amt}"
            );
        }
    }

    // ── Overflow defense ────────────────────────────────────────────────────

    #[test]
    fn mining_split_overflow_caught() {
        // u128::MAX * 500 overflows; result must be MathOverflow, never
        // silent wrap.
        let err = split_mining_reward(u128::MAX, MASTER).unwrap_err();
        assert_eq!(err, BankError::MathOverflow);
    }

    #[test]
    fn swap_split_overflow_caught() {
        let err = split_swap_output(u128::MAX, MASTER).unwrap_err();
        assert_eq!(err, BankError::MathOverflow);
    }

    // ── Constant audit ──────────────────────────────────────────────────────

    #[test]
    fn rate_constants_match_user_directive() {
        // Viktor's directive 2026-06-09: "5% of all mining coinbase and 0.3%
        // fee on dex" → master dev wallet 095b0e1f…3dd8. This test exists so any
        // future change to the constants requires an explicit decision — the test
        // name shows up in CI logs as "the rate-constant audit" which prompts a review.
        assert_eq!(MASTER_MINING_FEE_BPS, 500);
        assert_eq!(MASTER_SWAP_FEE_BPS, 30);
        assert_eq!(BPS_DENOMINATOR, 10_000);
    }
}
