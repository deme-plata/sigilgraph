//! sigil-dex — constant-product AMM math (`x * y = k`).
//!
//! Pure functions only. No I/O, no async, no persistence. The caller (today:
//! `sigil-tx::apply_tx`; tomorrow: `sigil-state::commit_state_transition`)
//! owns a `Pool` snapshot, calls a function here, and receives a
//! deterministic `Outcome` describing what the new pool state + user-side
//! credits should be. Whether that outcome gets written to flux-db is the
//! caller's problem.
//!
//! Why a separate crate (rather than burying this in `sigil-state` or
//! `sigil-tx`):
//!
//! 1. **Chokepoint stays narrow.** `sigil-state` exposes ~5 `pub(crate)`
//!    mutators. Letting it host swap math too means every overflow check
//!    lives next to the state machine. Splitting them keeps `sigil-state`
//!    focused on "what's in the maps + how do roots hash" and lets `sigil-dex`
//!    own "what does a swap mathematically produce."
//! 2. **Easier to property-test.** Pure functions, no state borrow, no
//!    `&mut SigilState`. Fuzz the math alone.
//! 3. **Mirrors the q-dex port path.** Quillon's q-dex shipped as one big
//!    async module; the cleaner shape for SIGIL is to peel the math out as a
//!    pure layer + put the I/O dance somewhere else.
//!
//! What this crate is NOT:
//!
//! - It does NOT debit user balances or credit them. Mutation routing is the
//!   caller's job — this crate only describes the *pool* delta + the
//!   *amount_out* a user would receive.
//! - It does NOT enforce minimum-reserve floors via consensus. The DEX-004
//!   floor is implemented as a per-pool reject if a remove would zero the
//!   reserves, but consensus-level floors (e.g. "swap must leave ≥ 1000
//!   units") live in `sigil-state` or a future `sigil-dex-policy` layer.
//! - It does NOT emit events. Events come from `sigil-events` and get
//!   attached at the `sigil-tx::apply_tx` layer.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// Fee denominator. Per-pool fee rates are expressed in basis points (bps),
/// where `30 bps = 0.30%`. The standard AMM fee is 30 bps; SIGIL allows
/// per-pool override at creation time.
pub const FEE_DENOMINATOR: u128 = 10_000;

/// Maximum fee a pool can charge. Higher than this and the AMM is functionally
/// confiscatory; capping at 10% bounds the harm a malicious pool creator can
/// do to swap counterparties (a swap through a 100% fee pool would silently
/// zero out the user's input).
pub const MAX_FEE_BPS: u16 = 1_000;

/// Minimum reserve. A pool whose post-swap reserves would dip below this is
/// rejected. Mirrors q-dex's `MIN_POOL_RESERVE`. Picked low for Phase 0; tune
/// once we have real swap volumes.
pub const MIN_RESERVE: u128 = 1_000;

/// Constant-product pool snapshot. Maps 1:1 to `sigil_state::PoolState` plus
/// a `fee_bps` field — `sigil-state`'s PoolState doesn't carry fees today;
/// for P5 the caller threads the bps in from the tx (or from a future
/// `fee_bps`-bearing PoolState extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    /// Token A reserve (in token A's smallest unit).
    pub reserve_a: u128,
    /// Token B reserve.
    pub reserve_b: u128,
    /// Total LP shares outstanding.
    pub total_shares: u128,
    /// Per-swap fee in basis points (e.g. `30` = 0.30%). Capped at
    /// [`MAX_FEE_BPS`].
    pub fee_bps: u16,
    /// Token-A fees accumulated since pool creation. Phase 0 treats these as
    /// part of the reserve (Uniswap V2 model) — they compound into the next
    /// swap automatically. The separate counter exists for analytics.
    pub accrued_fees_a: u128,
    /// Token-B fees accumulated since pool creation.
    pub accrued_fees_b: u128,
}

impl Pool {
    /// Empty pool with the given fee. Useful for tests; production pools come
    /// into being via the first `add_liquidity` call.
    pub fn empty(fee_bps: u16) -> Self {
        Self {
            reserve_a: 0,
            reserve_b: 0,
            total_shares: 0,
            fee_bps,
            accrued_fees_a: 0,
            accrued_fees_b: 0,
        }
    }

    /// Is this pool unbootstrapped (no LP shares yet)?
    pub fn is_empty(&self) -> bool {
        self.total_shares == 0
    }
}

/// Which side of the pool the swap inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapDirection {
    /// Caller deposits token A, receives token B.
    AtoB,
    /// Caller deposits token B, receives token A.
    BtoA,
}

/// Outcome of a successful `swap`. `pool_after` is the new snapshot the caller
/// should write back; `amount_out` is what the user receives; `fee_amount` is
/// the basis-point cut taken from `amount_in` and folded into the reserves
/// (Uniswap V2 — implicit LP reward).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapOutcome {
    /// New pool state to persist.
    pub pool_after: Pool,
    /// Amount of the *output* token the caller receives.
    pub amount_out: u128,
    /// Fee (in input-token units) deducted from `amount_in` before the
    /// constant-product math runs. Already folded into `pool_after`.
    pub fee_amount: u128,
}

/// Outcome of a successful `add_liquidity`. The caller credits
/// `shares_minted` LP tokens to the depositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidityOutcome {
    /// New pool state.
    pub pool_after: Pool,
    /// LP shares minted to the depositor.
    pub shares_minted: u128,
}

/// Outcome of a successful `remove_liquidity`. The caller credits
/// `amount_a` of token A and `amount_b` of token B back to the withdrawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawOutcome {
    /// New pool state.
    pub pool_after: Pool,
    /// Token A returned to the withdrawer.
    pub amount_a: u128,
    /// Token B returned to the withdrawer.
    pub amount_b: u128,
}

/// All the ways a DEX operation can fail. Variants map 1:1 onto q-dex's
/// guard rails (DEX-001..004) plus the overflow + sanity cases that u128
/// arithmetic forces us to be explicit about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DexError {
    /// Caller passed `amount_in = 0` (or `shares = 0` on a withdraw, or both
    /// deposit amounts as 0). Always a programming error upstream.
    #[error("zero amount")]
    ZeroAmount,

    /// Pool has no liquidity. Trying to swap or remove from an empty pool.
    #[error("pool is empty")]
    EmptyPool,

    /// A `checked_mul`/`checked_add`/`checked_sub` would have overflowed u128.
    /// Caller should reject the tx and tell the user to size down.
    #[error("math overflow")]
    MathOverflow,

    /// Swap output below the caller's slippage floor (DEX-003).
    #[error("slippage exceeded: expected at least {min_out}, got {actual}")]
    SlippageExceeded {
        /// Minimum the caller was willing to accept.
        min_out: u128,
        /// What the math actually produced.
        actual: u128,
    },

    /// Swap would push the output-side reserve below [`MIN_RESERVE`] (DEX-004).
    /// Or a withdraw would draw the pool below the floor.
    #[error("reserve floor violated: reserve would drop to {would_be}, floor is {floor}")]
    ReserveFloorViolated {
        /// Reserve level that would have resulted.
        would_be: u128,
        /// The configured floor.
        floor: u128,
    },

    /// `new_k < old_k` after the swap. Mathematically impossible if the fee
    /// is non-negative and the arithmetic is exact, so this is a defensive
    /// guard against bugs introduced by future changes.
    #[error("k-invariant violation: new_k < old_k")]
    KInvariantViolated,

    /// Pool fee exceeds [`MAX_FEE_BPS`].
    #[error("fee {fee_bps} bps exceeds max {max_bps} bps")]
    FeeTooHigh {
        /// Fee the caller asked for.
        fee_bps: u16,
        /// What the protocol allows.
        max_bps: u16,
    },

    /// Caller tried to remove more LP shares than they (or anyone) own.
    #[error("not enough shares: have {have}, requested {requested}")]
    InsufficientShares {
        /// Shares actually outstanding.
        have: u128,
        /// Shares the caller asked to burn.
        requested: u128,
    },

    /// `add_liquidity` to a non-empty pool with a ratio that doesn't match the
    /// current reserve ratio (within rounding). q-dex tolerates this by
    /// minting fewer shares than the larger side would justify; SIGIL Phase 0
    /// is strict — the caller must match the current ratio. Looser policy can
    /// land in a future patch.
    #[error("deposit ratio doesn't match current pool ratio")]
    RatioMismatch,
}

/// Apply a constant-product swap and return the resulting pool + amount_out.
///
/// Formula (lifted from q-dex `execute_atomic_swap`, simplified to integer
/// arithmetic):
///
/// ```text
/// amount_in_with_fee = amount_in * (FEE_DENOMINATOR - fee_bps)
///                                                / FEE_DENOMINATOR
/// amount_out         = amount_in_with_fee * reserve_out
///                       / (reserve_in + amount_in_with_fee)
/// ```
///
/// To avoid integer-truncation losses, the implementation computes the
/// numerator + denominator without intermediate division:
///
/// ```text
/// num = amount_in * (FEE_DENOMINATOR - fee_bps) * reserve_out
/// den = reserve_in * FEE_DENOMINATOR
///     + amount_in * (FEE_DENOMINATOR - fee_bps)
/// amount_out = num / den
/// ```
///
/// Both `num` and `den` use `checked_mul` / `checked_add` and bail out as
/// [`DexError::MathOverflow`] on u128 overflow. For Phase 0, callers should
/// size swaps so this doesn't trip — real production wants u256
/// (genesis lock #17 defers).
pub fn swap(
    pool: &Pool,
    direction: SwapDirection,
    amount_in: u128,
    min_amount_out: u128,
) -> Result<SwapOutcome, DexError> {
    if amount_in == 0 {
        return Err(DexError::ZeroAmount);
    }
    if pool.is_empty() {
        return Err(DexError::EmptyPool);
    }
    if pool.fee_bps > MAX_FEE_BPS {
        return Err(DexError::FeeTooHigh {
            fee_bps: pool.fee_bps,
            max_bps: MAX_FEE_BPS,
        });
    }

    let (reserve_in, reserve_out) = match direction {
        SwapDirection::AtoB => (pool.reserve_a, pool.reserve_b),
        SwapDirection::BtoA => (pool.reserve_b, pool.reserve_a),
    };

    if reserve_in == 0 || reserve_out == 0 {
        return Err(DexError::EmptyPool);
    }

    let fee_complement = FEE_DENOMINATOR
        .checked_sub(pool.fee_bps as u128)
        .ok_or(DexError::MathOverflow)?;

    let amount_in_with_fee = amount_in
        .checked_mul(fee_complement)
        .ok_or(DexError::MathOverflow)?;

    // The fee, expressed in input-token units. Truncates downward, which
    // means the actual fee may be off-by-one vs the exact rational; that's a
    // gain for the LPs, never a loss.
    let fee_amount = amount_in
        .checked_sub(amount_in_with_fee / FEE_DENOMINATOR)
        .ok_or(DexError::MathOverflow)?;

    let num = amount_in_with_fee
        .checked_mul(reserve_out)
        .ok_or(DexError::MathOverflow)?;

    let den = reserve_in
        .checked_mul(FEE_DENOMINATOR)
        .ok_or(DexError::MathOverflow)?
        .checked_add(amount_in_with_fee)
        .ok_or(DexError::MathOverflow)?;

    // `den > 0` is guaranteed because `reserve_in > 0` and FEE_DENOMINATOR > 0.
    let amount_out = num / den;

    if amount_out == 0 {
        return Err(DexError::SlippageExceeded {
            min_out: min_amount_out,
            actual: 0,
        });
    }

    if amount_out < min_amount_out {
        return Err(DexError::SlippageExceeded {
            min_out: min_amount_out,
            actual: amount_out,
        });
    }

    let new_reserve_in = reserve_in
        .checked_add(amount_in)
        .ok_or(DexError::MathOverflow)?;
    let new_reserve_out = reserve_out
        .checked_sub(amount_out)
        .ok_or(DexError::MathOverflow)?;

    if new_reserve_out < MIN_RESERVE {
        return Err(DexError::ReserveFloorViolated {
            would_be: new_reserve_out,
            floor: MIN_RESERVE,
        });
    }

    // k-invariant guard. The fee makes this redundant when math is exact, but
    // we keep it because (a) it costs one multiply per swap, and (b) it
    // catches future bugs introduced by changes to the fee formula.
    let old_k = reserve_in
        .checked_mul(reserve_out)
        .ok_or(DexError::MathOverflow)?;
    let new_k = new_reserve_in
        .checked_mul(new_reserve_out)
        .ok_or(DexError::MathOverflow)?;
    if new_k < old_k {
        return Err(DexError::KInvariantViolated);
    }

    // Map back to pool A/B orientation + fold the fee counter.
    let pool_after = match direction {
        SwapDirection::AtoB => Pool {
            reserve_a: new_reserve_in,
            reserve_b: new_reserve_out,
            total_shares: pool.total_shares,
            fee_bps: pool.fee_bps,
            accrued_fees_a: pool
                .accrued_fees_a
                .checked_add(fee_amount)
                .ok_or(DexError::MathOverflow)?,
            accrued_fees_b: pool.accrued_fees_b,
        },
        SwapDirection::BtoA => Pool {
            reserve_a: new_reserve_out,
            reserve_b: new_reserve_in,
            total_shares: pool.total_shares,
            fee_bps: pool.fee_bps,
            accrued_fees_a: pool.accrued_fees_a,
            accrued_fees_b: pool
                .accrued_fees_b
                .checked_add(fee_amount)
                .ok_or(DexError::MathOverflow)?,
        },
    };

    Ok(SwapOutcome {
        pool_after,
        amount_out,
        fee_amount,
    })
}

/// Deposit liquidity into a pool. If the pool is empty, this initializes it
/// with the deposit ratio + `fee_bps`; shares minted = `isqrt(amount_a *
/// amount_b)` (geometric mean, like Uniswap V2). If non-empty, the caller
/// must deposit `(amount_a, amount_b)` matching the current reserve ratio
/// within integer rounding; shares = `(amount_a * total_shares) / reserve_a`.
///
/// `init_fee_bps` is only consulted when the pool is empty — once a pool
/// exists, its fee is locked.
pub fn add_liquidity(
    pool: &Pool,
    amount_a: u128,
    amount_b: u128,
    init_fee_bps: u16,
) -> Result<LiquidityOutcome, DexError> {
    if amount_a == 0 || amount_b == 0 {
        return Err(DexError::ZeroAmount);
    }

    if pool.is_empty() {
        if init_fee_bps > MAX_FEE_BPS {
            return Err(DexError::FeeTooHigh {
                fee_bps: init_fee_bps,
                max_bps: MAX_FEE_BPS,
            });
        }
        let product = amount_a
            .checked_mul(amount_b)
            .ok_or(DexError::MathOverflow)?;
        let shares = isqrt_u128(product);
        if shares == 0 {
            return Err(DexError::ZeroAmount);
        }
        let pool_after = Pool {
            reserve_a: amount_a,
            reserve_b: amount_b,
            total_shares: shares,
            fee_bps: init_fee_bps,
            accrued_fees_a: 0,
            accrued_fees_b: 0,
        };
        return Ok(LiquidityOutcome {
            pool_after,
            shares_minted: shares,
        });
    }

    // Non-empty pool: enforce ratio match. q-dex tolerates ratio mismatch by
    // minting shares proportional to the smaller side; SIGIL Phase 0 is
    // strict to keep the chokepoint surface minimal. A future patch can relax
    // this.
    //
    // Ratio check: amount_a / reserve_a == amount_b / reserve_b
    //          ↔ amount_a * reserve_b == amount_b * reserve_a
    let lhs = amount_a
        .checked_mul(pool.reserve_b)
        .ok_or(DexError::MathOverflow)?;
    let rhs = amount_b
        .checked_mul(pool.reserve_a)
        .ok_or(DexError::MathOverflow)?;
    if lhs != rhs {
        return Err(DexError::RatioMismatch);
    }

    let shares = amount_a
        .checked_mul(pool.total_shares)
        .ok_or(DexError::MathOverflow)?
        / pool.reserve_a;

    if shares == 0 {
        return Err(DexError::ZeroAmount);
    }

    let new_reserve_a = pool
        .reserve_a
        .checked_add(amount_a)
        .ok_or(DexError::MathOverflow)?;
    let new_reserve_b = pool
        .reserve_b
        .checked_add(amount_b)
        .ok_or(DexError::MathOverflow)?;
    let new_total = pool
        .total_shares
        .checked_add(shares)
        .ok_or(DexError::MathOverflow)?;

    Ok(LiquidityOutcome {
        pool_after: Pool {
            reserve_a: new_reserve_a,
            reserve_b: new_reserve_b,
            total_shares: new_total,
            fee_bps: pool.fee_bps,
            accrued_fees_a: pool.accrued_fees_a,
            accrued_fees_b: pool.accrued_fees_b,
        },
        shares_minted: shares,
    })
}

/// Burn `shares` LP tokens and return the underlying `(amount_a, amount_b)`.
///
/// Caller is responsible for verifying the wallet actually holds `shares`
/// LP tokens (that lives in the LP-ledger, not the pool itself). This
/// function trusts the caller's claim and only checks against `total_shares`
/// outstanding.
///
/// Refuses to remove if doing so would drop either reserve below
/// [`MIN_RESERVE`] (DEX-004 floor) — *unless* the caller is removing all
/// outstanding shares, in which case the pool dies cleanly with zero
/// reserves. That escape hatch is what lets the last LP exit; without it
/// the floor would lock dust in the pool forever.
pub fn remove_liquidity(pool: &Pool, shares: u128) -> Result<WithdrawOutcome, DexError> {
    if shares == 0 {
        return Err(DexError::ZeroAmount);
    }
    if pool.is_empty() {
        return Err(DexError::EmptyPool);
    }
    if shares > pool.total_shares {
        return Err(DexError::InsufficientShares {
            have: pool.total_shares,
            requested: shares,
        });
    }

    let amount_a = pool
        .reserve_a
        .checked_mul(shares)
        .ok_or(DexError::MathOverflow)?
        / pool.total_shares;
    let amount_b = pool
        .reserve_b
        .checked_mul(shares)
        .ok_or(DexError::MathOverflow)?
        / pool.total_shares;

    let new_reserve_a = pool
        .reserve_a
        .checked_sub(amount_a)
        .ok_or(DexError::MathOverflow)?;
    let new_reserve_b = pool
        .reserve_b
        .checked_sub(amount_b)
        .ok_or(DexError::MathOverflow)?;
    let new_total = pool
        .total_shares
        .checked_sub(shares)
        .ok_or(DexError::MathOverflow)?;

    let draining_pool = new_total == 0;

    if !draining_pool && (new_reserve_a < MIN_RESERVE || new_reserve_b < MIN_RESERVE) {
        let would_be = new_reserve_a.min(new_reserve_b);
        return Err(DexError::ReserveFloorViolated {
            would_be,
            floor: MIN_RESERVE,
        });
    }

    Ok(WithdrawOutcome {
        pool_after: Pool {
            reserve_a: new_reserve_a,
            reserve_b: new_reserve_b,
            total_shares: new_total,
            fee_bps: pool.fee_bps,
            accrued_fees_a: pool.accrued_fees_a,
            accrued_fees_b: pool.accrued_fees_b,
        },
        amount_a,
        amount_b,
    })
}

/// Integer square root via Newton's method. Used for first-deposit LP shares
/// (geometric mean of `amount_a * amount_b`). Standard implementation —
/// converges in `O(log n)` iterations, terminates because the sequence is
/// monotonically non-increasing once it passes the true root.
///
/// Returns `floor(sqrt(n))`.
fn isqrt_u128(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    // Initial guess: 2^(ceil(log2(n)) / 2). Good enough that Newton converges
    // in ~7 steps for any u128.
    let mut x: u128 = 1u128 << ((128 - n.leading_zeros() as usize + 1) / 2);
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_30bps(reserve_a: u128, reserve_b: u128, total_shares: u128) -> Pool {
        Pool {
            reserve_a,
            reserve_b,
            total_shares,
            fee_bps: 30,
            accrued_fees_a: 0,
            accrued_fees_b: 0,
        }
    }

    // ── isqrt sanity ────────────────────────────────────────────────────────

    #[test]
    fn isqrt_known_values() {
        assert_eq!(isqrt_u128(0), 0);
        assert_eq!(isqrt_u128(1), 1);
        assert_eq!(isqrt_u128(4), 2);
        assert_eq!(isqrt_u128(9), 3);
        assert_eq!(isqrt_u128(15), 3);
        assert_eq!(isqrt_u128(16), 4);
        assert_eq!(isqrt_u128(10_000), 100);
        // (10^15)^2 = 10^30 — well within u128.
        assert_eq!(isqrt_u128(1_000_000_000_000_000_000_000_000_000_000), 1_000_000_000_000_000);
    }

    // ── add_liquidity ───────────────────────────────────────────────────────

    #[test]
    fn first_deposit_mints_geometric_mean_shares() {
        let pool = Pool::empty(30);
        let r = add_liquidity(&pool, 100_000, 100_000, 30).unwrap();
        // sqrt(100k * 100k) = 100k
        assert_eq!(r.shares_minted, 100_000);
        assert_eq!(r.pool_after.reserve_a, 100_000);
        assert_eq!(r.pool_after.reserve_b, 100_000);
        assert_eq!(r.pool_after.fee_bps, 30);
    }

    #[test]
    fn second_deposit_mints_proportional_shares() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        let r = add_liquidity(&pool, 50_000, 50_000, 30).unwrap();
        // shares = (50_000 * 100_000) / 100_000 = 50_000
        assert_eq!(r.shares_minted, 50_000);
        assert_eq!(r.pool_after.reserve_a, 150_000);
        assert_eq!(r.pool_after.reserve_b, 150_000);
        assert_eq!(r.pool_after.total_shares, 150_000);
    }

    #[test]
    fn deposit_with_mismatched_ratio_rejected() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        // 50k:60k doesn't match 1:1
        let err = add_liquidity(&pool, 50_000, 60_000, 30).unwrap_err();
        assert_eq!(err, DexError::RatioMismatch);
    }

    #[test]
    fn zero_deposit_rejected() {
        let pool = Pool::empty(30);
        assert_eq!(add_liquidity(&pool, 0, 100, 30).unwrap_err(), DexError::ZeroAmount);
        assert_eq!(add_liquidity(&pool, 100, 0, 30).unwrap_err(), DexError::ZeroAmount);
    }

    #[test]
    fn fee_too_high_rejected_on_creation() {
        let pool = Pool::empty(0);
        let err = add_liquidity(&pool, 100_000, 100_000, MAX_FEE_BPS + 1).unwrap_err();
        assert_eq!(
            err,
            DexError::FeeTooHigh {
                fee_bps: MAX_FEE_BPS + 1,
                max_bps: MAX_FEE_BPS,
            }
        );
    }

    // ── swap ────────────────────────────────────────────────────────────────

    #[test]
    fn swap_a_to_b_with_30bps_fee() {
        // 100k:100k pool, swap 1000 A in. With 30bps fee:
        // amount_in_with_fee = 1000 * 9970 = 9_970_000
        // num = 9_970_000 * 100_000 = 9.97e11
        // den = 100_000 * 10_000 + 9_970_000 = 1_009_970_000
        // amount_out = 9.97e11 / 1.00997e9 = 987.16... → floor = 987
        let pool = pool_30bps(100_000, 100_000, 100_000);
        let r = swap(&pool, SwapDirection::AtoB, 1000, 0).unwrap();
        assert_eq!(r.amount_out, 987);
        assert_eq!(r.pool_after.reserve_a, 101_000);
        assert_eq!(r.pool_after.reserve_b, 99_013);
        assert_eq!(r.fee_amount, 3); // 1000 * 30/10000 = 3
        assert_eq!(r.pool_after.accrued_fees_a, 3);
        assert_eq!(r.pool_after.accrued_fees_b, 0);
        assert_eq!(r.pool_after.fee_bps, 30);
    }

    #[test]
    fn swap_b_to_a_is_symmetric() {
        // Same pool, opposite direction — amount_out should be the same since
        // the pool is 1:1. fee_amount accrues on the OTHER side this time.
        let pool = pool_30bps(100_000, 100_000, 100_000);
        let r = swap(&pool, SwapDirection::BtoA, 1000, 0).unwrap();
        assert_eq!(r.amount_out, 987);
        assert_eq!(r.pool_after.reserve_a, 99_013);
        assert_eq!(r.pool_after.reserve_b, 101_000);
        assert_eq!(r.pool_after.accrued_fees_a, 0);
        assert_eq!(r.pool_after.accrued_fees_b, 3);
    }

    #[test]
    fn swap_into_empty_pool_rejected() {
        let pool = Pool::empty(30);
        assert_eq!(
            swap(&pool, SwapDirection::AtoB, 1000, 0).unwrap_err(),
            DexError::EmptyPool
        );
    }

    #[test]
    fn swap_zero_in_rejected() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        assert_eq!(
            swap(&pool, SwapDirection::AtoB, 0, 0).unwrap_err(),
            DexError::ZeroAmount
        );
    }

    #[test]
    fn swap_slippage_rejected_when_min_out_too_high() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        // Real out is 987; require 1000.
        let err = swap(&pool, SwapDirection::AtoB, 1000, 1000).unwrap_err();
        assert_eq!(
            err,
            DexError::SlippageExceeded {
                min_out: 1000,
                actual: 987,
            }
        );
    }

    #[test]
    fn swap_reserve_floor_violation() {
        // Tiny pool, big swap — out-side would drop below floor.
        let pool = pool_30bps(2_000, 2_000, 2_000);
        // Aim to drain almost all of B.
        let err = swap(&pool, SwapDirection::AtoB, 100_000, 0).unwrap_err();
        match err {
            DexError::ReserveFloorViolated { floor, .. } => assert_eq!(floor, MIN_RESERVE),
            other => panic!("expected ReserveFloorViolated, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_swap_preserves_k_within_fee() {
        // Send A→B, then B→A on the result. Both fees stick in the pool so
        // total k MUST strictly increase.
        let pool = pool_30bps(1_000_000, 1_000_000, 1_000_000);
        let k0 = pool.reserve_a * pool.reserve_b;
        let r1 = swap(&pool, SwapDirection::AtoB, 10_000, 0).unwrap();
        let r2 = swap(
            &r1.pool_after,
            SwapDirection::BtoA,
            r1.amount_out,
            0,
        )
        .unwrap();
        let k_final = r2.pool_after.reserve_a * r2.pool_after.reserve_b;
        assert!(k_final >= k0, "k must monotonically grow with fee swaps: {k_final} vs {k0}");
        // The user ends up with less A than they started — they paid two fees.
        // amount_out of the round trip is the A they get back from the B leg.
        assert!(r2.amount_out < 10_000);
    }

    #[test]
    fn swap_fee_zero_keeps_k_equal_within_truncation() {
        // 0 fee, large numbers — k can be unchanged or off by integer rounding
        // but never less. This proves the math itself is sound; the 30bps fee
        // is what guarantees k strictly grows.
        let pool = Pool {
            reserve_a: 1_000_000_000,
            reserve_b: 1_000_000_000,
            total_shares: 1_000_000_000,
            fee_bps: 0,
            accrued_fees_a: 0,
            accrued_fees_b: 0,
        };
        let k0 = pool.reserve_a * pool.reserve_b;
        let r = swap(&pool, SwapDirection::AtoB, 1_000_000, 0).unwrap();
        let k1 = r.pool_after.reserve_a * r.pool_after.reserve_b;
        // With 0 fee + truncation, k_after >= k_before by integer rounding.
        assert!(k1 >= k0);
        assert_eq!(r.fee_amount, 0);
    }

    #[test]
    fn fee_too_high_pool_rejects_swap() {
        let pool = Pool {
            fee_bps: MAX_FEE_BPS + 1,
            ..pool_30bps(100_000, 100_000, 100_000)
        };
        let err = swap(&pool, SwapDirection::AtoB, 1000, 0).unwrap_err();
        match err {
            DexError::FeeTooHigh { fee_bps, max_bps } => {
                assert_eq!(fee_bps, MAX_FEE_BPS + 1);
                assert_eq!(max_bps, MAX_FEE_BPS);
            }
            other => panic!("expected FeeTooHigh, got {other:?}"),
        }
    }

    // ── remove_liquidity ────────────────────────────────────────────────────

    #[test]
    fn remove_partial_liquidity_pro_rata() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        let r = remove_liquidity(&pool, 10_000).unwrap();
        assert_eq!(r.amount_a, 10_000);
        assert_eq!(r.amount_b, 10_000);
        assert_eq!(r.pool_after.reserve_a, 90_000);
        assert_eq!(r.pool_after.reserve_b, 90_000);
        assert_eq!(r.pool_after.total_shares, 90_000);
    }

    #[test]
    fn remove_all_drains_pool() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        let r = remove_liquidity(&pool, 100_000).unwrap();
        assert_eq!(r.amount_a, 100_000);
        assert_eq!(r.amount_b, 100_000);
        assert_eq!(r.pool_after.reserve_a, 0);
        assert_eq!(r.pool_after.reserve_b, 0);
        assert_eq!(r.pool_after.total_shares, 0);
        assert!(r.pool_after.is_empty());
    }

    #[test]
    fn remove_too_many_shares_rejected() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        let err = remove_liquidity(&pool, 100_001).unwrap_err();
        assert_eq!(
            err,
            DexError::InsufficientShares {
                have: 100_000,
                requested: 100_001,
            }
        );
    }

    #[test]
    fn remove_that_breaches_floor_without_draining_rejected() {
        // 1500 reserves, floor=1000, total_shares=1500. Removing 600 shares
        // leaves 900 < 1000 in each reserve while still having shares
        // outstanding → must reject.
        let pool = pool_30bps(1_500, 1_500, 1_500);
        let err = remove_liquidity(&pool, 600).unwrap_err();
        match err {
            DexError::ReserveFloorViolated { floor, .. } => assert_eq!(floor, MIN_RESERVE),
            other => panic!("expected ReserveFloorViolated, got {other:?}"),
        }
    }

    #[test]
    fn remove_zero_rejected() {
        let pool = pool_30bps(100_000, 100_000, 100_000);
        assert_eq!(remove_liquidity(&pool, 0).unwrap_err(), DexError::ZeroAmount);
    }

    #[test]
    fn remove_from_empty_pool_rejected() {
        let pool = Pool::empty(30);
        assert_eq!(
            remove_liquidity(&pool, 100).unwrap_err(),
            DexError::EmptyPool
        );
    }

    // ── wire format roundtrip ───────────────────────────────────────────────

    #[test]
    fn pool_json_roundtrip() {
        let pool = pool_30bps(123_456_789_012_345_678_901_234_567, 9_876, 555_555);
        let j = serde_json::to_string(&pool).unwrap();
        let p2: Pool = serde_json::from_str(&j).unwrap();
        assert_eq!(pool, p2);
    }

    #[test]
    fn swap_direction_json_roundtrip() {
        let d = SwapDirection::AtoB;
        let s = serde_json::to_string(&d).unwrap();
        let d2: SwapDirection = serde_json::from_str(&s).unwrap();
        assert_eq!(d, d2);
    }
}
