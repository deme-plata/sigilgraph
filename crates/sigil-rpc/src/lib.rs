//! sigil-rpc — the SIGIL node's money layer, as callable core functions.
//!
//! Two operations, both routed through the ONE auditable write surface
//! (`sigil_state::commit_state_transition`, which carries the 21M-cap +
//! k-invariant chokepoint):
//!
//!   * [`execute_swap`]     — exchange of money (AGORA / DEX) via `sigil-dex`.
//!   * [`submit_share`]     — mining: verify PoW, credit a coinbase reward,
//!                            with the cap enforced by the chokepoint (a reward
//!                            that would push total native supply past 21M is
//!                            REJECTED, not clamped).
//!
//! Transport (HTTP/JSON) wraps these; the lanes AGX-2 / MIN-2 plug a UI + the
//! in-tab miner into them. This crate is the part that must be correct: it
//! never pokes balances directly, only emits typed mutations the chokepoint
//! verifies.

pub mod presence;
pub mod nation;
pub mod nation_proof;
pub mod offline;
use sigil_dex::{Pool, SwapDirection, SwapOutcome};
use sigil_state::{
    commit_state_transition, PoolId, PoolState, SigilState, StateMutation, StateTransition,
    TokenId, WalletId, NATIVE,
};

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("no such pool")]
    NoPool,
    #[error("dex: {0}")]
    Dex(#[from] sigil_dex::DexError),
    #[error("commit: {0}")]
    Commit(#[from] sigil_state::CommitError),
    #[error("share does not meet difficulty target")]
    ShareBelowTarget,
    #[error("reward overflow")]
    Overflow,
    #[error("operator pool underfunded for this batch")]
    PoolUnderfunded,
    #[error("verification signature does not bind this wallet to the claimed identity")]
    InvalidVerification,
}

/// Verified-user onboarding + the earning gate (verified earns / unverified → dev-fee).
pub mod onboard;

/// Result of a light-verifier credit batch.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct LightCreditResult {
    /// Even-split amount each distinct verifier received.
    pub per_verifier: u128,
    /// Total debited from the operator pool (= per_verifier × num_verifiers).
    pub credited: u128,
    /// Number of DISTINCT verifiers credited (after sybil dedup).
    pub num_verifiers: u128,
}

/// Result of a swap: what the trader received (after the protocol fee) + the
/// LP fee folded into reserves + the sigil-bank protocol fee routed to the
/// master wallet.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct SwapResult {
    /// Output the trader actually keeps (gross output − protocol fee).
    pub amount_out: u128,
    /// AMM LP fee (input-token units), already compounded into the reserves.
    pub fee: u128,
    /// sigil-bank protocol fee (0.05% of gross output) routed to the master
    /// wallet. `0` if no master wallet is set.
    pub protocol_fee: u128,
}

fn to_dex_pool(ps: &PoolState) -> Pool {
    Pool {
        reserve_a: ps.reserve_a,
        reserve_b: ps.reserve_b,
        total_shares: ps.lp_shares,
        fee_bps: ps.fee_bps,
        accrued_fees_a: 0,
        accrued_fees_b: 0,
    }
}

/// Exchange of money: route `amount_in` through the constant-product pool and
/// settle the result as ONE `SwapDelta` mutation (the chokepoint expands it
/// into the trader's two balance moves + the pool update, verifying the
/// k-invariant in the process).
pub fn execute_swap(
    state: &mut SigilState,
    height: u64,
    from: WalletId,
    pool_id: PoolId,
    direction: SwapDirection,
    amount_in: u128,
    min_amount_out: u128,
) -> Result<SwapResult, RpcError> {
    let ps = state.pool(&pool_id).ok_or(RpcError::NoPool)?.clone();
    let dex_pool = to_dex_pool(&ps);
    let out: SwapOutcome = sigil_dex::swap(&dex_pool, direction, amount_in, min_amount_out)?;

    let (in_token, out_token): (TokenId, TokenId) = match direction {
        SwapDirection::AtoB => (ps.token_a, ps.token_b),
        SwapDirection::BtoA => (ps.token_b, ps.token_a),
    };

    let pool_after = PoolState {
        token_a: ps.token_a,
        token_b: ps.token_b,
        reserve_a: out.pool_after.reserve_a,
        reserve_b: out.pool_after.reserve_b,
        lp_shares: out.pool_after.total_shares,
        fee_bps: ps.fee_bps,
        accrued_fees: ps.accrued_fees.saturating_add(out.fee_amount),
    };

    // sigil-bank protocol fee: 0.05% of gross output → master wallet (treasury).
    // The SwapDelta credits the trader the FULL gross output (so it matches the
    // pool's k-invariant), then we move the master's cut in the output token.
    let split = sigil_bank::split_swap_output(out.amount_out, state.master_wallet())
        .map_err(|_| RpcError::Overflow)?;

    let mut mutations = vec![StateMutation::SwapDelta {
        from,
        pool: pool_id,
        in_token,
        in_amt: amount_in,
        out_token,
        out_amt: out.amount_out,
        // NATIVE gas fee slot — 0 in MVP (the protocol fee is taken from the
        // output below, sigil-bank style). The AMM LP fee is already folded
        // into pool_after's reserves by sigil-dex.
        fee: 0,
        pool_after,
    }];

    if split.master_share > 0 {
        if let Some(master) = state.master_wallet() {
            if master != from {
                // Balances are absolute; compute the post-SwapDelta values from
                // pre-state + the known credit (out.amount_out to the trader).
                let trader_out_pre = state.balance_of(&from, &out_token);
                let master_out_pre = state.balance_of(&master, &out_token);
                mutations.push(StateMutation::SetBalance {
                    wallet: from,
                    token: out_token,
                    amount: trader_out_pre + out.amount_out - split.master_share,
                });
                mutations.push(StateMutation::SetBalance {
                    wallet: master,
                    token: out_token,
                    amount: master_out_pre + split.master_share,
                });
            }
        }
    }

    commit_state_transition(state, &StateTransition { at_height: height, mutations }, height)?;
    Ok(SwapResult {
        amount_out: split.user_share,
        fee: out.fee_amount,
        protocol_fee: split.master_share,
    })
}

fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut n = 0u32;
    for &b in bytes {
        if b == 0 {
            n += 8;
        } else {
            n += b.leading_zeros();
            break;
        }
    }
    n
}

/// Mining: verify a proof-of-work share, then credit a coinbase `reward` to the
/// miner's native balance. The 21M cap is enforced by the chokepoint inside
/// `commit_state_transition` — this function NEVER bypasses it.
///
/// PoW: `blake3(header || nonce_le)` must have at least `difficulty_bits`
/// leading zero bits. (MIN-2 aligns the in-tab BLAKE4-lite miner to this hash.)
pub fn submit_share(
    state: &mut SigilState,
    height: u64,
    miner: WalletId,
    header: &[u8],
    nonce: u64,
    difficulty_bits: u32,
    reward: u128,
) -> Result<u128, RpcError> {
    let mut buf = header.to_vec();
    buf.extend_from_slice(&nonce.to_le_bytes());
    let digest = blake3::hash(&buf);
    if leading_zero_bits(digest.as_bytes()) < difficulty_bits {
        return Err(RpcError::ShareBelowTarget);
    }

    // PoW verified — credit through the ONE cap-enforced money chokepoint.
    credit_share(state, height, miner, reward)
}

/// Credit a coinbase `reward` for an ALREADY-VERIFIED mining share to `miner`,
/// with the 21M native cap enforced by the state chokepoint (a reward that would
/// push total native supply past MAX_SUPPLY is REJECTED, not clamped). Returns
/// the miner's new NATIVE balance.
///
/// This is the credit half of [`submit_share`] factored out so BOTH lanes share
/// one money path: the single-lane BLAKE3 PoW (via [`submit_share`]) AND the
/// dual-lane BLAKE4 Φ + VDF Ω block (verified node-side by flux-miner's
/// `check_submission`, then credited here). The verification rule differs per
/// lane; the WRITE is identical and audited in exactly one place.
pub fn credit_share(
    state: &mut SigilState,
    height: u64,
    miner: WalletId,
    reward: u128,
) -> Result<u128, RpcError> {
    let new_balance = state
        .balance_of(&miner, &NATIVE)
        .checked_add(reward)
        .ok_or(RpcError::Overflow)?;

    let transition = StateTransition {
        at_height: height,
        mutations: vec![StateMutation::SetBalance {
            wallet: miner,
            token: NATIVE,
            amount: new_balance,
        }],
    };
    // If this reward would push total native supply past MAX_SUPPLY, the
    // chokepoint returns Err here and the miner is NOT credited.
    commit_state_transition(state, &transition, height)?;
    Ok(new_balance)
}

/// Light-miner wallet credit (P1, balance-integrity-first). Viktor's policy:
/// **even-split** of the **0.1% operator pool** among **distinct** verifiers,
/// **batched** every N blocks. Because it is a pure transfer (operator pool →
/// verifiers), total native supply is UNCHANGED — the 21M cap holds by
/// construction. Sybil-proof: duplicate verifiers are deduped, and even-split
/// means more identities only dilute the per-verifier share, never multiply it.
///
/// Genesis-node guard: the operator pool is the ONLY debit and can never go
/// negative (returns [`RpcError::PoolUnderfunded`] instead).
///
/// IDEMPOTENCY is the caller's responsibility: this is a transfer, so calling
/// it twice for the same (height, verifier set) double-credits. The batched
/// driver MUST gate on a processed-batch marker per height before calling.
pub fn credit_light_verifiers(
    state: &mut SigilState,
    height: u64,
    operator_pool: WalletId,
    pool_amount: u128,
    verifiers: &[WalletId],
) -> Result<LightCreditResult, RpcError> {
    // Sybil dedup: 1,000 fake light-miners collapse to their distinct wallets.
    let mut distinct: Vec<WalletId> = verifiers.to_vec();
    distinct.sort();
    distinct.dedup();
    let n = distinct.len() as u128;
    if n == 0 || pool_amount == 0 {
        return Ok(LightCreditResult { per_verifier: 0, credited: 0, num_verifiers: n });
    }
    let per = pool_amount / n; // integer; remainder stays in the pool
    if per == 0 {
        return Ok(LightCreditResult { per_verifier: 0, credited: 0, num_verifiers: n });
    }
    let total = per * n;

    let pool_bal = state.balance_of(&operator_pool, &NATIVE);
    if pool_bal < total {
        return Err(RpcError::PoolUnderfunded); // pool can never go negative
    }

    // All amounts read from PRE-state; the chokepoint applies absolute sets.
    let mut mutations = Vec::with_capacity(distinct.len() + 1);
    mutations.push(StateMutation::SetBalance {
        wallet: operator_pool,
        token: NATIVE,
        amount: pool_bal - total,
    });
    for v in &distinct {
        let b = state.balance_of(v, &NATIVE);
        mutations.push(StateMutation::SetBalance { wallet: *v, token: NATIVE, amount: b + per });
    }
    commit_state_transition(state, &StateTransition { at_height: height, mutations }, height)?;
    Ok(LightCreditResult { per_verifier: per, credited: total, num_verifiers: n })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_A: TokenId = [0xAA; 32];
    const TOKEN_B: TokenId = [0xBB; 32];
    const POOL: PoolId = [0xCC; 32];
    const TRADER: WalletId = [0x11; 32];
    const MINER: WalletId = [0x22; 32];

    fn genesis() -> SigilState {
        let mut s = SigilState::new();
        // Seed a trader with TOKEN_A and bootstrap a 100k/100k pool.
        let t = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetBalance { wallet: TRADER, token: TOKEN_A, amount: 1_000_000 },
                StateMutation::SetPool {
                    pool: POOL,
                    state: PoolState {
                        token_a: TOKEN_A,
                        token_b: TOKEN_B,
                        reserve_a: 100_000,
                        reserve_b: 100_000,
                        lp_shares: 100_000,
                        fee_bps: 30,
                        accrued_fees: 0,
                    },
                },
            ],
        };
        commit_state_transition(&mut s, &t, 0).expect("genesis commits");
        s
    }

    #[test]
    fn swap_exchanges_money() {
        let mut s = genesis();
        let before_b = s.balance_of(&TRADER, &TOKEN_B);
        let r = execute_swap(&mut s, 1, TRADER, POOL, SwapDirection::AtoB, 10_000, 1)
            .expect("swap settles");
        assert!(r.amount_out > 0, "trader receives token B");
        assert_eq!(s.balance_of(&TRADER, &TOKEN_B), before_b + r.amount_out);
        assert!(s.balance_of(&TRADER, &TOKEN_A) < 1_000_000, "token A spent");
    }

    #[test]
    fn mining_credits_under_cap() {
        let mut s = genesis();
        // difficulty 0 → any share passes; this tests the credit + cap path.
        let bal = submit_share(&mut s, 1, MINER, b"sigil-block-1", 42, 0, 50).expect("reward credits");
        assert_eq!(bal, 50);
        assert_eq!(s.balance_of(&MINER, &NATIVE), 50);
    }

    #[test]
    fn mining_cannot_breach_21m_cap() {
        let mut s = genesis();
        // A reward larger than the whole supply must be REJECTED by the chokepoint.
        let huge = u128::MAX;
        let res = submit_share(&mut s, 1, MINER, b"sigil-block-1", 42, 0, huge);
        assert!(res.is_err(), "chokepoint rejects supply-breaching reward");
    }

    // ── Light-miner credit (P1) — chronos-sim-first: conservation, even-split,
    //    sybil bound, pool floor. ──────────────────────────────────────────────
    const OP: WalletId = [0xEE; 32];
    const V1: WalletId = [0x01; 32];
    const V2: WalletId = [0x02; 32];
    const V3: WalletId = [0x03; 32];

    fn pool_state(bal: u128) -> SigilState {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance { wallet: OP, token: NATIVE, amount: bal }],
        };
        commit_state_transition(&mut s, &t, 0).expect("seed operator pool");
        s
    }

    fn native_supply(s: &SigilState) -> u128 {
        [OP, V1, V2, V3].iter().map(|w| s.balance_of(w, &NATIVE)).sum()
    }

    #[test]
    fn light_credit_even_split_and_conservation() {
        let mut s = pool_state(1_000_000);
        let before = native_supply(&s);
        let r = credit_light_verifiers(&mut s, 1, OP, 900, &[V1, V2, V3]).unwrap();
        assert_eq!(r.num_verifiers, 3);
        assert_eq!(r.per_verifier, 300);
        assert_eq!(s.balance_of(&V1, &NATIVE), 300);
        assert_eq!(s.balance_of(&V2, &NATIVE), 300);
        assert_eq!(s.balance_of(&V3, &NATIVE), 300);
        assert_eq!(s.balance_of(&OP, &NATIVE), 1_000_000 - 900);
        // CONSERVATION: pure transfer, total native supply unchanged → cap holds.
        assert_eq!(native_supply(&s), before);
    }

    #[test]
    fn light_credit_sybil_dedup() {
        let mut s = pool_state(1_000_000);
        // One wallet spinning 3 light-miners must NOT triple-earn.
        let r = credit_light_verifiers(&mut s, 1, OP, 900, &[V1, V1, V1]).unwrap();
        assert_eq!(r.num_verifiers, 1, "deduped to one distinct verifier");
        assert_eq!(s.balance_of(&V1, &NATIVE), 900, "credited once, not 3×");
    }

    #[test]
    fn light_credit_pool_floor() {
        let mut s = pool_state(500);
        // Batch wants more than the pool holds → rejected, pool never goes negative.
        let res = credit_light_verifiers(&mut s, 1, OP, 900, &[V1, V2, V3]);
        assert!(matches!(res, Err(RpcError::PoolUnderfunded)));
        assert_eq!(s.balance_of(&OP, &NATIVE), 500, "pool untouched on reject");
    }

    const MASTER: WalletId = [0xFF; 32];

    #[test]
    fn swap_protocol_fee_routes_to_master() {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetMasterWallet { wallet: MASTER },
                StateMutation::SetBalance { wallet: TRADER, token: TOKEN_A, amount: 1_000_000 },
                StateMutation::SetPool {
                    pool: POOL,
                    state: PoolState {
                        token_a: TOKEN_A, token_b: TOKEN_B,
                        reserve_a: 100_000, reserve_b: 100_000,
                        lp_shares: 100_000, fee_bps: 30, accrued_fees: 0,
                    },
                },
            ],
        };
        commit_state_transition(&mut s, &t, 0).unwrap();

        let r = execute_swap(&mut s, 1, TRADER, POOL, SwapDirection::AtoB, 10_000, 1).unwrap();
        assert!(r.protocol_fee > 0, "treasury takes a protocol cut");
        // master credited the fee, trader gets output minus the fee
        assert_eq!(s.balance_of(&MASTER, &TOKEN_B), r.protocol_fee, "master credited");
        assert_eq!(s.balance_of(&TRADER, &TOKEN_B), r.amount_out, "trader gets net output");
        // 0.05% (5 bps) of the gross output
        let gross = r.amount_out + r.protocol_fee;
        assert_eq!(r.protocol_fee, gross * 5 / 10_000);
    }
}
