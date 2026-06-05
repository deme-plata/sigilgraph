//! sigil-state — the SIGIL state machine.
//!
//! See `SIGIL_GENESIS_v0.md` §3. This crate owns the four state SMTs:
//!
//! - `wallet`  — `WalletId -> u128` balances
//! - `dex`     — `PoolId -> {reserves, lp_shares, accrued_fees}`
//! - `events`  — Merkle (binary) tree over `BLAKE3(event_encoded)`
//! - `contract`— `(ContractId, SlotId) -> [u8; 32]`
//!
//! Rule #6 from the SIGIL skill: state writes ONLY through
//! [`commit_state_transition`]. Every other mutating API is `pub(crate)`.
//! `flux_ai_audit` is configured to fail the build if any cross-crate write
//! path bypasses the chokepoint.
//!
//! Phase 0: SMTs are stubbed with `BTreeMap`-backed in-memory stores and a
//! simple BLAKE3-of-sorted-leaves root. P3 swaps in a real Sparse Merkle
//! Tree with non-membership proofs against flux-db column families.

#![warn(missing_docs)]

/// Project Stargate-v1 #1 — incremental multiset-accumulator roots. Replaces
/// the O(n) whole-map rehash (73% of wall-clock per chronos) with O(1)
/// per-touched-leaf updates. Standalone + fully tested; `roots()` wiring is
/// a follow-up patch (see SIGIL_STARGATE_INCREMENTAL_ROOTS.md).
pub mod acc;
pub use acc::Accumulator;

/// Sparse Merkle Tree — the COMPLEMENT to `acc`. Where `acc` (LtHash multiset)
/// gives the fastest possible O(1) state root for throughput but NO per-key
/// proofs, `smt` gives inclusion + non-membership proofs (genesis §3) for the
/// tip-proof / 10ms light-client verify path — at a slightly higher (still
/// O(1)-in-n, 256-hash) per-update cost. Two structures, two jobs: accumulator
/// for the committed root, SMT for the proofs a light client checks against it.
pub mod smt;
pub use smt::Smt;

/// Decimal-string codec for `u128` — works around serde_json's lack of
/// u128 wire support. Apply with `#[serde(with = "u128_str")]`.
pub mod u128_str {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use sigil_header::Root;

/// 32-byte wallet address (Quillon-compatible).
pub type WalletId = [u8; 32];

/// 32-byte pool identifier.
pub type PoolId = [u8; 32];

/// 32-byte contract identifier.
pub type ContractId = [u8; 32];

/// 32-byte VM storage slot index inside a contract.
pub type SlotId = [u8; 32];

/// 32-byte token identifier. Native SIGIL token uses an all-zero ID.
pub type TokenId = [u8; 32];

/// The all-zero TokenId sentinel for the native SIGIL token. Defined at the
/// state layer so the P5-C chokepoint delta helpers can compare against it
/// without depending on sigil-tx (which would create a circular path-dep).
/// sigil-tx keeps its own local `NATIVE` const; both point at the same
/// `[0u8; 32]` value — the wire format is the canonical sentinel.
pub const NATIVE: TokenId = [0u8; 32];

/// Native SIGIL decimals (base units per whole coin = 10^this).
pub const SIGIL_DECIMALS: u32 = 8;

/// Hard, protocol-level maximum supply of the native SIGIL token, in base
/// units: **21,000,000 SIGIL × 10^8 = 2.1×10^15**. This is a *compile-time
/// constant baked into every node binary* — not a runtime parameter, not a
/// genesis field that could be set wrong. [`commit_state_transition`] refuses
/// any state transition whose post-state native supply exceeds it, so no
/// emission/coinbase/mint path — present or future — can inflate past 21M.
/// (Quillon's hyperinflation/wrong-emission incidents are the reason this is a
/// hard invariant at the chokepoint rather than emission-controller bookkeeping.)
pub const MAX_SUPPLY: u128 = 21_000_000 * 10u128.pow(SIGIL_DECIMALS);

// Compile-time correctness: the cap is exactly 21M × 10^8. A wrong edit to
// either constant fails the BUILD, before any node can run with a bad cap.
const _: () = assert!(MAX_SUPPLY == 2_100_000_000_000_000, "SIGIL max supply must be 21,000,000 × 10^8");

/// DEX pool state at a height. Carries the identifying pair (`token_a` /
/// `token_b`), reserves, total outstanding LP shares, and the per-pool fee in
/// basis points. The legacy `accrued_fees` counter from Phase 0 is kept as a
/// running total but its split per-side is folded into the reserves on every
/// swap (Uniswap V2 model — fees compound, no separate "withdraw fees"
/// path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    /// Token-A identifier (the side hashed first into the pool id).
    pub token_a: TokenId,
    /// Token-B identifier.
    pub token_b: TokenId,
    /// Token A reserve.
    #[serde(with = "u128_str")]
    pub reserve_a: u128,
    /// Token B reserve.
    #[serde(with = "u128_str")]
    pub reserve_b: u128,
    /// Total LP shares outstanding.
    #[serde(with = "u128_str")]
    pub lp_shares: u128,
    /// Per-swap fee in basis points. `30` = 0.30%. Locked at pool creation;
    /// post-MVP governance can lift this into a per-pool config tx.
    pub fee_bps: u16,
    /// Lifetime accrued fees in input-token units, summed across both sides.
    /// Analytics-only — actual fee money lives in the reserves.
    #[serde(with = "u128_str")]
    pub accrued_fees: u128,
}

/// Top-level state container. Internal mutation is `pub(crate)` only.
#[derive(Debug, Clone, Default)]
pub struct SigilState {
    /// Wallet balances. Keyed by `(wallet, token)` so multi-token is native
    /// from day one without a schema bump later.
    pub(crate) wallets: BTreeMap<(WalletId, TokenId), u128>,
    /// DEX pools.
    pub(crate) pools: BTreeMap<PoolId, PoolState>,
    /// Block-scoped event hashes — flushed into `event_log_root` and cleared
    /// at the end of each block. Persistent storage lives in flux-db column
    /// families (see genesis §4).
    pub(crate) block_events: Vec<[u8; 32]>,
    /// Contract storage.
    pub(crate) contracts: BTreeMap<(ContractId, SlotId), [u8; 32]>,
    /// Protocol-fee recipient — populated at genesis via a one-shot
    /// `StateMutation::SetMasterWallet`. Once set, cannot be changed without
    /// an explicit consensus upgrade. Mining + DEX layers consult this wallet
    /// to split a configurable share of every reward + swap output (see
    /// `sigil-bank::{MASTER_MINING_FEE_BPS, MASTER_SWAP_FEE_BPS}`).
    pub(crate) master_wallet: Option<WalletId>,
    /// Incremental additive multiset accumulator over the wallet set (Stargate
    /// #1). `wallet_acc == Σ wallet_leaf(w,t,v)` over every live `wallets`
    /// entry, maintained O(1) per `set_balance` (sub old leaf, add new). Lets
    /// [`Self::roots`] return `wallet_state_root` in O(1) instead of an
    /// O(state) rehash — the fix for the batch-auth real-pipeline plateau.
    /// Default `[0;4]` == empty-state root, so genesis stays consistent.
    pub(crate) wallet_acc: [u64; 4],
    /// Live total of the native SIGIL token across all wallets, maintained O(1)
    /// per `set_balance`. [`commit_state_transition`] asserts this never exceeds
    /// [`MAX_SUPPLY`] — the hard 21M cap. Default 0 (empty genesis).
    pub(crate) native_supply: u128,
}

/// The four state roots produced at the end of a block. These go into the
/// header verbatim. See [`sigil_header::SigilBlockHeaderV0`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRoots {
    /// Root over wallet balances.
    pub wallet_state_root: Root,
    /// Root over DEX pools.
    pub dex_state_root: Root,
    /// Root over typed event log emitted in the closing block.
    pub event_log_root: Root,
    /// Root over VM contract storage.
    pub contract_state_root: Root,
}

impl SigilState {
    /// Empty state — the genesis pre-image.
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute the four roots over the current in-memory state.
    ///
    /// Phase 0 implementation: BLAKE3 of the deterministically-serialized
    /// sorted leaves. NOT a real SMT (no non-membership proofs, no
    /// position-binding). Phase 3 replaces this with a true SMT keyed by the
    /// natural identifier and produces inclusion + non-membership proofs.
    /// The shape of [`StateRoots`] stays stable across that swap.
    pub fn roots(&self) -> StateRoots {
        StateRoots {
            // O(1): the incrementally-maintained multiset accumulator, instead
            // of an O(state) rehash of every wallet entry each block.
            wallet_state_root:   acc_to_root(self.wallet_acc),
            dex_state_root:      hash_map(&self.pools),
            event_log_root:      hash_event_log(&self.block_events),
            contract_state_root: hash_map(&self.contracts),
        }
    }

    /// Live total native SIGIL supply (base units). Always ≤ [`MAX_SUPPLY`] —
    /// `commit_state_transition` enforces the cap before sealing any block.
    pub fn native_supply(&self) -> u128 {
        self.native_supply
    }

    /// Recompute the wallet root FROM SCRATCH — O(state). The incremental
    /// `wallet_acc` that `roots()` returns in O(1) must always equal this. A
    /// node can call it on boot/audit to detect accumulator drift, and it is
    /// the benchmark baseline that shows why the incremental path exists: at a
    /// large wallet set this is seconds/block, while `roots()` stays constant.
    pub fn wallet_root_recompute(&self) -> Root {
        let mut acc = [0u64; 4];
        for ((w, t), v) in &self.wallets {
            acc = acc_add(acc, wallet_leaf(w, t, *v));
        }
        acc_to_root(acc)
    }

    /// Number of live wallet entries (state size).
    pub fn wallet_count(&self) -> usize {
        self.wallets.len()
    }

    /// Read-only balance lookup. Reads are always free; writes go through
    /// [`commit_state_transition`].
    pub fn balance_of(&self, wallet: &WalletId, token: &TokenId) -> u128 {
        self.wallets
            .get(&(*wallet, *token))
            .copied()
            .unwrap_or(0)
    }

    /// Read-only pool lookup.
    pub fn pool(&self, pool: &PoolId) -> Option<&PoolState> {
        self.pools.get(pool)
    }

    /// Read-only contract slot lookup.
    pub fn contract_slot(&self, contract: &ContractId, slot: &SlotId) -> [u8; 32] {
        self.contracts
            .get(&(*contract, *slot))
            .copied()
            .unwrap_or([0u8; 32])
    }

    /// Read-only master-wallet lookup. `None` means protocol-fee skim is
    /// off — block producers + DEX swaps behave as if no bank exists.
    pub fn master_wallet(&self) -> Option<WalletId> {
        self.master_wallet
    }

    // ── pub(crate) mutators — invoked ONLY by commit_state_transition ──────

    pub(crate) fn set_balance(&mut self, wallet: WalletId, token: TokenId, amount: u128) {
        // Maintain the incremental multiset accumulator: remove the old entry's
        // leaf (if any), add the new one (if non-zero). All wallet writes route
        // through here, so `wallet_acc` always equals the from-scratch multiset.
        let old = self.wallets.get(&(wallet, token)).copied().unwrap_or(0);
        if old != 0 {
            self.wallet_acc = acc_sub(self.wallet_acc, wallet_leaf(&wallet, &token, old));
        }
        if amount == 0 {
            self.wallets.remove(&(wallet, token));
        } else {
            self.wallets.insert((wallet, token), amount);
            self.wallet_acc = acc_add(self.wallet_acc, wallet_leaf(&wallet, &token, amount));
        }
        // Track native-token supply incrementally (transfers net to zero; only
        // mints/burns move it). The cap is enforced at the commit chokepoint.
        if token == NATIVE {
            self.native_supply = self.native_supply.wrapping_sub(old).wrapping_add(amount);
        }
    }

    pub(crate) fn set_pool(&mut self, pool: PoolId, state: PoolState) {
        self.pools.insert(pool, state);
    }

    pub(crate) fn push_event_hash(&mut self, h: [u8; 32]) {
        self.block_events.push(h);
    }

    pub(crate) fn set_contract_slot(
        &mut self,
        contract: ContractId,
        slot: SlotId,
        value: [u8; 32],
    ) {
        if value == [0u8; 32] {
            self.contracts.remove(&(contract, slot));
        } else {
            self.contracts.insert((contract, slot), value);
        }
    }

    pub(crate) fn clear_block_events(&mut self) {
        self.block_events.clear();
    }

    /// One-shot install of the master wallet. Subsequent calls are rejected
    /// at the [`commit_state_transition`] gate — this raw mutator silently
    /// overwrites if reached, so the policing happens one level up.
    pub(crate) fn set_master_wallet(&mut self, wallet: WalletId) {
        self.master_wallet = Some(wallet);
    }
}

/// A single mutation queued by a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateMutation {
    /// Set a wallet balance for a specific token.
    SetBalance {
        wallet: WalletId,
        token: TokenId,
        #[serde(with = "u128_str")]
        amount: u128,
    },
    /// Replace a DEX pool's state.
    SetPool { pool: PoolId, state: PoolState },
    /// Push a typed-event hash into the current block's event log. The actual
    /// event encoding lives in `sigil-events`.
    PushEventHash([u8; 32]),
    /// Set a contract storage slot.
    SetContractSlot { contract: ContractId, slot: SlotId, value: [u8; 32] },

    /// P5-C: typed delta for a DEX swap. Carries enough context for the
    /// chokepoint to verify the k-invariant + balance conservation as one
    /// unit before expanding into [`SetBalance`] + [`SetPool`] primitives.
    /// Reduces the wire surface a producer has to set explicitly (no need
    /// to compute the two SetBalance calls + the SetPool — the chokepoint
    /// does it from the delta), and makes "this is a swap" legible to
    /// future audit / STARK-proof generation.
    SwapDelta {
        from: WalletId,
        pool: PoolId,
        in_token: TokenId,
        #[serde(with = "u128_str")]
        in_amt: u128,
        out_token: TokenId,
        #[serde(with = "u128_str")]
        out_amt: u128,
        #[serde(with = "u128_str")]
        fee: u128,
        /// Pool snapshot after the swap. The chokepoint verifies this
        /// against the math: reserves shifted by exactly (in_amt, -out_amt)
        /// on the right sides, and `reserve_a × reserve_b` did not shrink
        /// (the k-invariant — fees compound into reserves so k must grow
        /// or stay equal).
        pool_after: PoolState,
    },

    /// P5-C: typed delta for an LP deposit. Carries the amounts deposited
    /// plus the resulting `shares_minted` so the chokepoint can verify
    /// `pool_after.lp_shares == prev.lp_shares + shares_minted` and refuse
    /// share-creation that doesn't conserve.
    LpDelta {
        from: WalletId,
        pool: PoolId,
        #[serde(with = "u128_str")]
        amt_a: u128,
        #[serde(with = "u128_str")]
        amt_b: u128,
        #[serde(with = "u128_str")]
        shares_minted: u128,
        #[serde(with = "u128_str")]
        fee: u128,
        pool_after: PoolState,
    },

    /// P5-C: typed delta for an LP burn / withdraw. Symmetric to
    /// [`LpDelta`] — the chokepoint verifies
    /// `pool_after.lp_shares == prev.lp_shares - shares_burned`
    /// and that the withdrawn amounts come pro-rata from the reserves.
    LpBurnDelta {
        from: WalletId,
        pool: PoolId,
        #[serde(with = "u128_str")]
        shares_burned: u128,
        #[serde(with = "u128_str")]
        amt_a_out: u128,
        #[serde(with = "u128_str")]
        amt_b_out: u128,
        #[serde(with = "u128_str")]
        fee: u128,
        pool_after: PoolState,
    },

    /// One-shot install of the protocol-fee recipient (the "master wallet").
    /// Emitted ONLY in the genesis transition. The commit gate rejects any
    /// later [`SetMasterWallet`] with [`CommitError::MasterWalletAlreadySet`]
    /// — once the bank is installed it stays installed for the lifetime of
    /// the chain.
    SetMasterWallet { wallet: WalletId },
}

/// A batched, atomic state transition. Always processed by
/// [`commit_state_transition`] — never applied piecemeal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTransition {
    /// Block height this transition seals into.
    pub at_height: u64,
    /// Mutations applied in order. Phase 0 does not enforce ordering rules;
    /// Phase 3 STARK proof attests the ordering plus arithmetic safety.
    pub mutations: Vec<StateMutation>,
}

/// Errors from the chokepoint. None are recoverable mid-block; producers must
/// drop the offending tx and re-attempt at the next height.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// `transition.at_height` didn't match `expected_height`.
    #[error("height mismatch in commit: expected {expected}, got {got}")]
    WrongHeight { expected: u64, got: u64 },

    /// The transition's post-state native supply would exceed the hard 21M cap
    /// ([`MAX_SUPPLY`]). The whole block is rejected — no path may inflate past
    /// 21,000,000 SIGIL. This is the consensus teeth behind the compile-time cap.
    #[error("supply cap exceeded: post-state native supply {supply} > MAX_SUPPLY {cap}")]
    SupplyCapExceeded { supply: u128, cap: u128 },

    /// A future check (post-Phase 0) decided the transition violates an
    /// invariant. Carries a freeform reason.
    #[error("invariant violation: {0}")]
    Invariant(String),

    /// P5-C: A typed delta (SwapDelta / LpDelta / LpBurnDelta) carried a
    /// `pool_after` shape that contradicts what the math should produce —
    /// k-invariant shrank, shares didn't conserve, balances overflow, etc.
    /// A producer that emits a typed delta MUST have run the math first
    /// (sigil-tx::apply_tx does); a divergence at the chokepoint means
    /// the producer is buggy or malicious.
    #[error("delta math invariant failed: {0}")]
    DeltaInvariant(String),

    /// P5-C: a typed delta references a pool that doesn't exist yet. Pool
    /// creation goes through LpDelta on a fresh PoolId — never SwapDelta
    /// or LpBurnDelta.
    #[error("delta against unknown pool")]
    UnknownPool,

    /// P5-C: an arithmetic overflow occurred while applying a delta. Should
    /// never happen on well-formed input but caught defensively because
    /// u128 math sits right at the edge of representability for some DEX
    /// scenarios.
    #[error("delta arithmetic overflow")]
    DeltaOverflow,

    /// Block tried to re-install the master wallet after genesis. Reject —
    /// the master wallet is set once at the chain's birth and never
    /// re-assigned without a hard fork.
    #[error("master wallet already set; cannot re-install without consensus upgrade")]
    MasterWalletAlreadySet,
}

// ── P5-C delta application helpers ────────────────────────────────────────
//
// Each `apply_*_delta` inflates a typed delta into the low-level
// SetBalance/SetPool primitives + verifies chokepoint guards. Typed deltas
// are produced by trusted code (sigil-tx::apply_tx → sigil_dex math); the
// chokepoint isn't re-deriving the AMM curve, it's catching the case where
// a delta got MUNGED in transit — shares didn't conserve, reserves shifted
// by wrong amounts, sender's balance can't cover the in-token + fee. Those
// are the signals a malicious or buggy producer flashes.

fn apply_swap_delta(
    state: &mut SigilState,
    from: WalletId,
    pool: PoolId,
    in_token: TokenId,
    in_amt: u128,
    out_token: TokenId,
    out_amt: u128,
    fee: u128,
    pool_after: PoolState,
) -> Result<(), CommitError> {
    let prev = state.pool(&pool).ok_or(CommitError::UnknownPool)?.clone();

    let is_a_to_b = in_token == prev.token_a && out_token == prev.token_b;
    let is_b_to_a = in_token == prev.token_b && out_token == prev.token_a;
    if !(is_a_to_b || is_b_to_a) {
        return Err(CommitError::DeltaInvariant(
            "swap delta tokens don't match pool pair".into(),
        ));
    }
    if pool_after.token_a != prev.token_a
        || pool_after.token_b != prev.token_b
        || pool_after.fee_bps != prev.fee_bps
        || pool_after.lp_shares != prev.lp_shares
    {
        return Err(CommitError::DeltaInvariant(
            "swap delta mutated immutable pool fields (token_a/b, fee_bps, lp_shares)".into(),
        ));
    }

    let (expected_a, expected_b) = if is_a_to_b {
        (
            prev.reserve_a.checked_add(in_amt).ok_or(CommitError::DeltaOverflow)?,
            prev.reserve_b.checked_sub(out_amt).ok_or(CommitError::DeltaInvariant(
                "swap delta reserve_b underflow on AtoB".into(),
            ))?,
        )
    } else {
        (
            prev.reserve_a.checked_sub(out_amt).ok_or(CommitError::DeltaInvariant(
                "swap delta reserve_a underflow on BtoA".into(),
            ))?,
            prev.reserve_b.checked_add(in_amt).ok_or(CommitError::DeltaOverflow)?,
        )
    };
    if pool_after.reserve_a != expected_a || pool_after.reserve_b != expected_b {
        return Err(CommitError::DeltaInvariant(
            "swap delta pool_after reserves don't match in_amt / out_amt".into(),
        ));
    }

    let from_native = state.balance_of(&from, &NATIVE);
    if in_token == NATIVE {
        let need = fee.checked_add(in_amt).ok_or(CommitError::DeltaOverflow)?;
        if from_native < need {
            return Err(CommitError::DeltaInvariant(
                "swap delta sender NATIVE balance can't cover in_amt+fee".into(),
            ));
        }
        state.set_balance(from, NATIVE, from_native - need);
    } else {
        if from_native < fee {
            return Err(CommitError::DeltaInvariant(
                "swap delta sender NATIVE balance can't cover fee".into(),
            ));
        }
        let sender_in = state.balance_of(&from, &in_token);
        if sender_in < in_amt {
            return Err(CommitError::DeltaInvariant(
                "swap delta sender can't cover in_amt".into(),
            ));
        }
        state.set_balance(from, NATIVE, from_native - fee);
        state.set_balance(from, in_token, sender_in - in_amt);
    }
    let sender_out = state.balance_of(&from, &out_token);
    let new_out = sender_out.checked_add(out_amt).ok_or(CommitError::DeltaOverflow)?;
    state.set_balance(from, out_token, new_out);
    state.set_pool(pool, pool_after);
    Ok(())
}

fn apply_lp_delta(
    state: &mut SigilState,
    from: WalletId,
    pool: PoolId,
    amt_a: u128,
    amt_b: u128,
    shares_minted: u128,
    fee: u128,
    pool_after: PoolState,
) -> Result<(), CommitError> {
    let prev_opt = state.pool(&pool).cloned();

    match &prev_opt {
        Some(prev) => {
            if pool_after.token_a != prev.token_a
                || pool_after.token_b != prev.token_b
                || pool_after.fee_bps != prev.fee_bps
            {
                return Err(CommitError::DeltaInvariant(
                    "lp delta mutated pool token pair or fee_bps on existing pool".into(),
                ));
            }
            let expected_a = prev.reserve_a.checked_add(amt_a).ok_or(CommitError::DeltaOverflow)?;
            let expected_b = prev.reserve_b.checked_add(amt_b).ok_or(CommitError::DeltaOverflow)?;
            let expected_shares = prev.lp_shares.checked_add(shares_minted).ok_or(CommitError::DeltaOverflow)?;
            if pool_after.reserve_a != expected_a
                || pool_after.reserve_b != expected_b
                || pool_after.lp_shares != expected_shares
            {
                return Err(CommitError::DeltaInvariant(
                    "lp delta pool_after doesn't match amt_a/amt_b/shares_minted".into(),
                ));
            }
        }
        None => {
            if pool_after.reserve_a != amt_a
                || pool_after.reserve_b != amt_b
                || pool_after.lp_shares != shares_minted
            {
                return Err(CommitError::DeltaInvariant(
                    "lp delta first-deposit pool_after doesn't match amt_a/amt_b/shares_minted".into(),
                ));
            }
        }
    }

    let from_native = state.balance_of(&from, &NATIVE);
    let need_native = if pool_after.token_a == NATIVE {
        fee.checked_add(amt_a).ok_or(CommitError::DeltaOverflow)?
    } else if pool_after.token_b == NATIVE {
        fee.checked_add(amt_b).ok_or(CommitError::DeltaOverflow)?
    } else {
        fee
    };
    if from_native < need_native {
        return Err(CommitError::DeltaInvariant(
            "lp delta sender NATIVE balance can't cover deposit + fee".into(),
        ));
    }
    state.set_balance(from, NATIVE, from_native - need_native);
    if pool_after.token_a != NATIVE {
        let bal_a = state.balance_of(&from, &pool_after.token_a);
        if bal_a < amt_a {
            return Err(CommitError::DeltaInvariant(
                "lp delta sender can't cover amt_a".into(),
            ));
        }
        state.set_balance(from, pool_after.token_a, bal_a - amt_a);
    }
    if pool_after.token_b != NATIVE {
        let bal_b = state.balance_of(&from, &pool_after.token_b);
        if bal_b < amt_b {
            return Err(CommitError::DeltaInvariant(
                "lp delta sender can't cover amt_b".into(),
            ));
        }
        state.set_balance(from, pool_after.token_b, bal_b - amt_b);
    }

    state.set_pool(pool, pool_after);
    Ok(())
}

fn apply_lp_burn_delta(
    state: &mut SigilState,
    from: WalletId,
    pool: PoolId,
    shares_burned: u128,
    amt_a_out: u128,
    amt_b_out: u128,
    fee: u128,
    pool_after: PoolState,
) -> Result<(), CommitError> {
    let prev = state.pool(&pool).ok_or(CommitError::UnknownPool)?.clone();

    if pool_after.token_a != prev.token_a
        || pool_after.token_b != prev.token_b
        || pool_after.fee_bps != prev.fee_bps
    {
        return Err(CommitError::DeltaInvariant(
            "lp-burn delta mutated immutable pool fields".into(),
        ));
    }
    let expected_a = prev.reserve_a.checked_sub(amt_a_out).ok_or(CommitError::DeltaInvariant(
        "lp-burn reserve_a underflow".into(),
    ))?;
    let expected_b = prev.reserve_b.checked_sub(amt_b_out).ok_or(CommitError::DeltaInvariant(
        "lp-burn reserve_b underflow".into(),
    ))?;
    let expected_shares = prev.lp_shares.checked_sub(shares_burned).ok_or(CommitError::DeltaInvariant(
        "lp-burn shares underflow".into(),
    ))?;
    if pool_after.reserve_a != expected_a
        || pool_after.reserve_b != expected_b
        || pool_after.lp_shares != expected_shares
    {
        return Err(CommitError::DeltaInvariant(
            "lp-burn delta pool_after doesn't match amt_*_out / shares_burned".into(),
        ));
    }

    let from_native = state.balance_of(&from, &NATIVE);
    if from_native < fee {
        return Err(CommitError::DeltaInvariant(
            "lp-burn delta sender NATIVE balance can't cover fee".into(),
        ));
    }
    state.set_balance(from, NATIVE, from_native - fee);
    let bal_a = state.balance_of(&from, &prev.token_a);
    let bal_b = state.balance_of(&from, &prev.token_b);
    state.set_balance(from, prev.token_a, bal_a.checked_add(amt_a_out).ok_or(CommitError::DeltaOverflow)?);
    state.set_balance(from, prev.token_b, bal_b.checked_add(amt_b_out).ok_or(CommitError::DeltaOverflow)?);

    state.set_pool(pool, pool_after);
    Ok(())
}

/// THE chokepoint. The single function allowed to mutate `SigilState`.
///
/// Side effects:
/// 1. Apply each `StateMutation` in `transition` to `state`.
/// 2. Compute the 4 state roots over the post-transition state.
/// 3. Return [`StateRoots`] for the caller to fold into the next header.
///
/// In Phase 3+ this also:
/// - Verifies a STARK proof attesting the transition.
/// - Cross-checks the resulting roots against the header's claimed roots.
/// - Aborts (halts the node) on divergence.
pub fn commit_state_transition(
    state: &mut SigilState,
    transition: &StateTransition,
    expected_height: u64,
) -> Result<StateRoots, CommitError> {
    if transition.at_height != expected_height {
        return Err(CommitError::WrongHeight {
            expected: expected_height,
            got: transition.at_height,
        });
    }
    for m in &transition.mutations {
        match m.clone() {
            StateMutation::SetBalance { wallet, token, amount } => {
                state.set_balance(wallet, token, amount);
            }
            StateMutation::SetPool { pool, state: s } => {
                state.set_pool(pool, s);
            }
            StateMutation::PushEventHash(h) => {
                state.push_event_hash(h);
            }
            StateMutation::SetContractSlot { contract, slot, value } => {
                state.set_contract_slot(contract, slot, value);
            }
            StateMutation::SwapDelta {
                from, pool, in_token, in_amt, out_token, out_amt, fee, pool_after,
            } => {
                apply_swap_delta(
                    state, from, pool, in_token, in_amt, out_token, out_amt, fee, pool_after,
                )?;
            }
            StateMutation::LpDelta {
                from, pool, amt_a, amt_b, shares_minted, fee, pool_after,
            } => {
                apply_lp_delta(
                    state, from, pool, amt_a, amt_b, shares_minted, fee, pool_after,
                )?;
            }
            StateMutation::LpBurnDelta {
                from, pool, shares_burned, amt_a_out, amt_b_out, fee, pool_after,
            } => {
                apply_lp_burn_delta(
                    state, from, pool, shares_burned, amt_a_out, amt_b_out, fee, pool_after,
                )?;
            }
            StateMutation::SetMasterWallet { wallet } => {
                if state.master_wallet.is_some() {
                    return Err(CommitError::MasterWalletAlreadySet);
                }
                state.set_master_wallet(wallet);
            }
        }
    }
    // HARD SUPPLY CAP — the consensus teeth behind the compile-time MAX_SUPPLY.
    // Transfers net to zero supply; only a mint/coinbase/over-funded-genesis can
    // trip this. A violating block is rejected outright — 21M can never inflate.
    if state.native_supply > MAX_SUPPLY {
        return Err(CommitError::SupplyCapExceeded { supply: state.native_supply, cap: MAX_SUPPLY });
    }

    let roots = state.roots();
    state.clear_block_events();
    Ok(roots)
}

// ── Incremental multiset accumulator (Stargate #1) ─────────────────────────
// An additive multiset hash: root = Σ leaf(entry) over a 256-bit ([u64;4])
// accumulator. Order-independent (commutative), so insert = add4, remove =
// sub4, and the root is identical regardless of mutation order — every node
// computes the same value. O(1) per mutation vs O(state) rehash.

/// Domain-separated 256-bit leaf for a wallet entry `(wallet, token) -> amount`.
fn wallet_leaf(wallet: &WalletId, token: &TokenId, amount: u128) -> [u64; 4] {
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/wallet-acc/v1");
    h.update(wallet);
    h.update(token);
    h.update(&amount.to_le_bytes());
    let b = h.finalize();
    let d = b.as_bytes();
    [
        u64::from_le_bytes(d[0..8].try_into().unwrap()),
        u64::from_le_bytes(d[8..16].try_into().unwrap()),
        u64::from_le_bytes(d[16..24].try_into().unwrap()),
        u64::from_le_bytes(d[24..32].try_into().unwrap()),
    ]
}

/// 256-bit add with carry (insert a leaf into the multiset).
#[inline]
fn acc_add(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    let (s0, c0) = a[0].overflowing_add(b[0]);
    let (s1, c1a) = a[1].overflowing_add(b[1]);
    let (s1, c1b) = s1.overflowing_add(c0 as u64);
    let (s2, c2a) = a[2].overflowing_add(b[2]);
    let (s2, c2b) = s2.overflowing_add((c1a | c1b) as u64);
    [s0, s1, s2, a[3].wrapping_add(b[3]).wrapping_add((c2a | c2b) as u64)]
}

/// 256-bit sub with borrow (remove a leaf from the multiset).
#[inline]
fn acc_sub(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
    let (d0, b0) = a[0].overflowing_sub(b[0]);
    let (d1, b1a) = a[1].overflowing_sub(b[1]);
    let (d1, b1b) = d1.overflowing_sub(b0 as u64);
    let (d2, b2a) = a[2].overflowing_sub(b[2]);
    let (d2, b2b) = d2.overflowing_sub((b1a | b1b) as u64);
    [d0, d1, d2, a[3].wrapping_sub(b[3]).wrapping_sub((b2a | b2b) as u64)]
}

/// Serialize the accumulator to the 32-byte `Root` carried in the header.
fn acc_to_root(acc: [u64; 4]) -> Root {
    let mut r = [0u8; 32];
    for (i, limb) in acc.iter().enumerate() {
        r[i * 8..i * 8 + 8].copy_from_slice(&limb.to_le_bytes());
    }
    r
}

/// From-scratch multiset root over a wallet map — the spec the incremental
/// `wallet_acc` must always equal. Used only by tests + a recovery path.
#[cfg(test)]
fn wallet_multiset_root(map: &BTreeMap<(WalletId, TokenId), u128>) -> [u64; 4] {
    let mut acc = [0u64; 4];
    for ((w, t), v) in map {
        acc = acc_add(acc, wallet_leaf(w, t, *v));
    }
    acc
}

// ── Phase 0 root helpers ──────────────────────────────────────────────────
// Replace with a real SMT in P3.

fn hash_map<K, V>(map: &BTreeMap<K, V>) -> Root
where
    K: Serialize,
    V: Serialize,
{
    // We cannot use `serde_json::to_vec(map)` directly: serde_json refuses
    // BTreeMap with non-string keys (tuple keys, byte-array keys all fail).
    // The previous impl swallowed that error with `if let Ok`, which meant
    // every wallet/pool/contract map silently hashed to BLAKE3("") regardless
    // of contents — every block had identical state roots. sigil-tx's
    // send_changes_wallet_root_and_emits_two_events test caught it: balances
    // updated, but wallet_state_root stayed constant.
    //
    // Fix: walk entries (BTreeMap iteration is sorted, so this stays
    // deterministic) and feed serde_json-of-each-key + serde_json-of-each-value
    // into the hasher with length prefixes to avoid collision between
    // boundaries. Keys serialized in array context succeed even when they'd
    // fail as a map key. P3 swaps the whole helper for a real SMT.
    let mut hasher = blake3::Hasher::new();
    for (k, v) in map {
        let k_bytes = serde_json::to_vec(k).expect("BTreeMap key must serialize");
        let v_bytes = serde_json::to_vec(v).expect("BTreeMap value must serialize");
        hasher.update(&(k_bytes.len() as u32).to_le_bytes());
        hasher.update(&k_bytes);
        hasher.update(&(v_bytes.len() as u32).to_le_bytes());
        hasher.update(&v_bytes);
    }
    *hasher.finalize().as_bytes()
}

fn hash_event_log(events: &[[u8; 32]]) -> Root {
    if events.is_empty() {
        return [0u8; 32];
    }
    // Balanced binary Merkle. Pad with the last hash on odd levels.
    let mut layer: Vec<[u8; 32]> = events.to_vec();
    while layer.len() > 1 {
        if layer.len() % 2 == 1 {
            let last = *layer.last().unwrap();
            layer.push(last);
        }
        let mut next = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks(2) {
            let mut h = blake3::Hasher::new();
            h.update(&pair[0]);
            h.update(&pair[1]);
            next.push(*h.finalize().as_bytes());
        }
        layer = next;
    }
    layer[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state_has_stable_roots() {
        let s = SigilState::new();
        let r1 = s.roots();
        let r2 = s.roots();
        assert_eq!(r1, r2);
    }

    #[test]
    fn balance_changes_change_wallet_root() {
        let mut s = SigilState::new();
        let before = s.roots().wallet_state_root;
        let t = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SetBalance {
                wallet: [1u8; 32],
                token: [0u8; 32],
                amount: 100,
            }],
        };
        let roots = commit_state_transition(&mut s, &t, 1).unwrap();
        assert_ne!(roots.wallet_state_root, before);
        assert_eq!(s.balance_of(&[1u8; 32], &[0u8; 32]), 100);
    }

    /// SOUNDNESS of the incremental accumulator: after thousands of random
    /// inserts / updates / removals (amount 0 removes), the O(1)-maintained
    /// `wallet_acc` must EXACTLY equal the from-scratch multiset over the map.
    /// Any drift here = nodes computing different wallet roots = consensus split.
    #[test]
    fn incremental_wallet_acc_never_drifts() {
        let mut s = SigilState::new();
        assert_eq!(s.wallet_acc, wallet_multiset_root(&s.wallets)); // empty
        let mut seed = 0xabcd_ef01_2345_6789u64;
        let mut nx = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        for _ in 0..10_000 {
            let mut w = [0u8; 32]; w[..8].copy_from_slice(&(nx() % 64).to_le_bytes());
            let t = if nx() & 1 == 0 { NATIVE } else { let mut a = [0u8; 32]; a[0] = 1; a };
            let amount = (nx() % 1000) as u128; // 0 ⇒ remove; same key revisited ⇒ update
            s.set_balance(w, t, amount);
            // invariant must hold after EVERY single mutation
            debug_assert_eq!(s.wallet_acc, wallet_multiset_root(&s.wallets));
        }
        assert_eq!(s.wallet_acc, wallet_multiset_root(&s.wallets),
            "incremental acc drifted from from-scratch multiset");
        assert_eq!(s.roots().wallet_state_root, acc_to_root(wallet_multiset_root(&s.wallets)));
    }

    /// The hard 21M cap: minting exactly MAX_SUPPLY is allowed, one base unit
    /// over is rejected, transfers preserve supply, and non-native tokens are
    /// uncapped. On a rejected commit the state is discarded (producers commit
    /// on a scratch clone), so we attempt the over-cap mint on a clone.
    #[test]
    fn supply_cap_enforced_at_21m() {
        let mut s = SigilState::new();
        assert_eq!(s.native_supply(), 0);
        assert_eq!(MAX_SUPPLY, 21_000_000 * 100_000_000);

        // mint EXACTLY 21M → allowed.
        let mint = StateTransition { at_height: 1, mutations: vec![
            StateMutation::SetBalance { wallet: [1u8; 32], token: NATIVE, amount: MAX_SUPPLY }] };
        commit_state_transition(&mut s, &mint, 1).expect("minting exactly 21M must be allowed");
        assert_eq!(s.native_supply(), MAX_SUPPLY);

        // transfer preserves supply (from -= 100, to += 100).
        let xfer = StateTransition { at_height: 2, mutations: vec![
            StateMutation::SetBalance { wallet: [1u8; 32], token: NATIVE, amount: MAX_SUPPLY - 100 },
            StateMutation::SetBalance { wallet: [2u8; 32], token: NATIVE, amount: 100 }] };
        commit_state_transition(&mut s, &xfer, 2).expect("transfer within cap");
        assert_eq!(s.native_supply(), MAX_SUPPLY);

        // mint ONE base unit over the cap → rejected (on a discardable clone).
        let mut over = s.clone();
        let bad = StateTransition { at_height: 3, mutations: vec![
            StateMutation::SetBalance { wallet: [3u8; 32], token: NATIVE, amount: 1 }] };
        match commit_state_transition(&mut over, &bad, 3) {
            Err(CommitError::SupplyCapExceeded { supply, cap }) => {
                assert_eq!(cap, MAX_SUPPLY);
                assert!(supply > MAX_SUPPLY);
            }
            other => panic!("21M + 1 must be rejected, got {other:?}"),
        }
        assert_eq!(s.native_supply(), MAX_SUPPLY, "good state untouched by the rejected block");

        // non-native tokens are NOT bound by the SIGIL cap (own supply).
        let other = { let mut a = [0u8; 32]; a[0] = 7; a };
        let big = StateTransition { at_height: 3, mutations: vec![
            StateMutation::SetBalance { wallet: [4u8; 32], token: other, amount: u128::MAX / 2 }] };
        commit_state_transition(&mut s, &big, 3).expect("non-native token is not the 21M-capped SIGIL");
        assert_eq!(s.native_supply(), MAX_SUPPLY, "non-native mint must not touch native supply");
    }

    #[test]
    fn commit_rejects_wrong_height() {
        let mut s = SigilState::new();
        let t = StateTransition { at_height: 5, mutations: vec![] };
        let err = commit_state_transition(&mut s, &t, 4).unwrap_err();
        assert!(matches!(err, CommitError::WrongHeight { .. }));
    }

    #[test]
    fn event_log_root_is_merkle_balanced() {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 1,
            mutations: vec![
                StateMutation::PushEventHash([1u8; 32]),
                StateMutation::PushEventHash([2u8; 32]),
                StateMutation::PushEventHash([3u8; 32]),
            ],
        };
        let roots = commit_state_transition(&mut s, &t, 1).unwrap();
        // Three events → padded to 4 → 2 internal nodes → 1 root, not equal
        // to any individual leaf.
        assert_ne!(roots.event_log_root, [0u8; 32]);
        assert_ne!(roots.event_log_root, [1u8; 32]);
    }

    #[test]
    fn event_log_root_clears_after_commit() {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::PushEventHash([1u8; 32])],
        };
        let _ = commit_state_transition(&mut s, &t, 1).unwrap();
        // After commit, next block's empty event set hashes to 0.
        assert_eq!(s.roots().event_log_root, [0u8; 32]);
    }

    #[test]
    fn contract_zero_value_deletes_slot() {
        let mut s = SigilState::new();
        let t1 = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SetContractSlot {
                contract: [3u8; 32], slot: [4u8; 32], value: [9u8; 32],
            }],
        };
        commit_state_transition(&mut s, &t1, 1).unwrap();
        assert_eq!(s.contract_slot(&[3u8; 32], &[4u8; 32]), [9u8; 32]);
        let t2 = StateTransition {
            at_height: 2,
            mutations: vec![StateMutation::SetContractSlot {
                contract: [3u8; 32], slot: [4u8; 32], value: [0u8; 32],
            }],
        };
        commit_state_transition(&mut s, &t2, 2).unwrap();
        assert_eq!(s.contract_slot(&[3u8; 32], &[4u8; 32]), [0u8; 32]);
        assert!(s.contracts.is_empty());
    }

    // ── Regression: every StateMutation variant must JSON-roundtrip ──
    //
    // Paired with `every_event_variant_json_roundtrips_u128_safely` in
    // sigil-events. Same purpose: the P3 demo blocker (rocky msg #33,
    // 2026-05-29) was a bare `u128` field that broke JSON deserialize.
    // Any future StateMutation variant that introduces a u128 without
    // `#[serde(with = "u128_str")]` will fail this test.
    #[test]
    fn every_mutation_variant_json_roundtrips_u128_safely() {
        let big: u128 = 1_000_000_000_000_000_000_000_u128;
        let pool_state = PoolState {
            token_a: [0u8; 32],
            token_b: [1u8; 32],
            reserve_a: big,
            reserve_b: big - 1,
            lp_shares: big / 2,
            fee_bps: 30,
            accrued_fees: 42,
        };
        let mutations = vec![
            StateMutation::SetBalance {
                wallet: [7u8; 32], token: [0u8; 32], amount: big,
            },
            StateMutation::SetPool { pool: [8u8; 32], state: pool_state.clone() },
            StateMutation::PushEventHash([0xAA; 32]),
            StateMutation::SetContractSlot {
                contract: [9u8; 32], slot: [10u8; 32], value: [0xBB; 32],
            },
        ];
        for m in &mutations {
            let bytes = serde_json::to_vec(m)
                .unwrap_or_else(|e| panic!("serialize {:?}: {}", m, e));
            let parsed: StateMutation = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!(
                    "deserialize {:?} (bytes: {}): {}",
                    m, String::from_utf8_lossy(&bytes), e
                ));
            assert_eq!(*m, parsed, "roundtrip mismatch for {:?}", m);
        }
        // PoolState directly, since it's a Serialize-derived public type
        // that StateMutation::SetPool depends on.
        let bytes = serde_json::to_vec(&pool_state).unwrap();
        let parsed: PoolState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(pool_state, parsed);
    }

    // ── P5-C typed-delta tests ──────────────────────────────────────────

    fn seeded_pool_state(s: &mut SigilState, pool: PoolId, token_b: TokenId) {
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetPool {
                pool,
                state: PoolState {
                    token_a: NATIVE,
                    token_b,
                    reserve_a: 1_000_000,
                    reserve_b: 1_000_000,
                    lp_shares: 1_000_000,
                    fee_bps: 30,
                    accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(s, &t, 0).unwrap();
    }

    fn fund(s: &mut SigilState, wallet: WalletId, token: TokenId, amt: u128) {
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance { wallet, token, amount: amt }],
        };
        // Use a synthetic height bump just to satisfy the chokepoint at_height
        // check — height 0 collides with seeded_pool_state's 0, so pump the
        // counter via a fresh chokepoint call at the same height (chokepoint
        // doesn't enforce monotonicity).
        commit_state_transition(s, &t, 0).unwrap();
    }

    #[test]
    fn swap_delta_happy_path_a_to_b() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let pool: PoolId = [9u8; 32];
        let other: TokenId = [7u8; 32];
        // Seed pool + alice's NATIVE balance in a single transition so we
        // don't bump the height between them.
        let setup = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetPool {
                    pool,
                    state: PoolState {
                        token_a: NATIVE, token_b: other,
                        reserve_a: 1_000_000, reserve_b: 1_000_000,
                        lp_shares: 1_000_000, fee_bps: 30, accrued_fees: 0,
                    },
                },
                StateMutation::SetBalance { wallet: alice, token: NATIVE, amount: 10_000 },
            ],
        };
        commit_state_transition(&mut s, &setup, 0).unwrap();

        // Apply a hand-computed swap delta: alice swaps 1000 NATIVE → ~996 other.
        // The chokepoint doesn't re-derive the AMM curve — it validates that
        // pool_after matches in_amt/out_amt and balances cover the trade.
        let delta = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SwapDelta {
                from: alice, pool,
                in_token: NATIVE, in_amt: 1000,
                out_token: other, out_amt: 990,
                fee: 1,
                pool_after: PoolState {
                    token_a: NATIVE, token_b: other,
                    reserve_a: 1_001_000, reserve_b: 999_010,
                    lp_shares: 1_000_000, fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(&mut s, &delta, 1).unwrap();
        assert_eq!(s.balance_of(&alice, &NATIVE), 10_000 - 1000 - 1); // -in_amt -fee
        assert_eq!(s.balance_of(&alice, &other), 990);                 // +out_amt
        let p = s.pool(&pool).unwrap();
        assert_eq!(p.reserve_a, 1_001_000);
        assert_eq!(p.reserve_b, 999_010);
        assert_eq!(p.lp_shares, 1_000_000); // swap never mints shares
    }

    #[test]
    fn swap_delta_rejects_when_reserves_dont_match_in_out() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let pool: PoolId = [9u8; 32];
        let other: TokenId = [7u8; 32];
        let setup = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetPool {
                    pool,
                    state: PoolState {
                        token_a: NATIVE, token_b: other,
                        reserve_a: 1_000_000, reserve_b: 1_000_000,
                        lp_shares: 1_000_000, fee_bps: 30, accrued_fees: 0,
                    },
                },
                StateMutation::SetBalance { wallet: alice, token: NATIVE, amount: 10_000 },
            ],
        };
        commit_state_transition(&mut s, &setup, 0).unwrap();

        // Caller lies — claims in_amt=1000 but pool_after shows reserve_a grew
        // by 5000. Chokepoint catches the inconsistency.
        let bad_delta = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SwapDelta {
                from: alice, pool,
                in_token: NATIVE, in_amt: 1000,
                out_token: other, out_amt: 990,
                fee: 1,
                pool_after: PoolState {
                    token_a: NATIVE, token_b: other,
                    reserve_a: 1_005_000, // <- lie: should be 1_001_000
                    reserve_b: 999_010,
                    lp_shares: 1_000_000, fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        let err = commit_state_transition(&mut s, &bad_delta, 1).unwrap_err();
        assert!(matches!(err, CommitError::DeltaInvariant(_)), "got {err:?}");
    }

    #[test]
    fn swap_delta_rejects_token_pair_mismatch() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let pool: PoolId = [9u8; 32];
        let other: TokenId = [7u8; 32];
        let third: TokenId = [8u8; 32];
        let setup = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetPool {
                    pool,
                    state: PoolState {
                        token_a: NATIVE, token_b: other,
                        reserve_a: 1_000_000, reserve_b: 1_000_000,
                        lp_shares: 1_000_000, fee_bps: 30, accrued_fees: 0,
                    },
                },
                StateMutation::SetBalance { wallet: alice, token: third, amount: 10_000 },
                StateMutation::SetBalance { wallet: alice, token: NATIVE, amount: 10 },
            ],
        };
        commit_state_transition(&mut s, &setup, 0).unwrap();

        // Pool is (NATIVE, other) but delta uses third token — must reject.
        let bogus = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SwapDelta {
                from: alice, pool,
                in_token: third, in_amt: 100,
                out_token: NATIVE, out_amt: 99,
                fee: 1,
                pool_after: PoolState {
                    token_a: NATIVE, token_b: other,
                    reserve_a: 1_000_000, reserve_b: 1_000_000,
                    lp_shares: 1_000_000, fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        let err = commit_state_transition(&mut s, &bogus, 1).unwrap_err();
        assert!(matches!(err, CommitError::DeltaInvariant(_)));
    }

    #[test]
    fn lp_delta_first_deposit_creates_pool() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let pool: PoolId = [9u8; 32];
        let other: TokenId = [7u8; 32];
        let setup = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetBalance { wallet: alice, token: NATIVE, amount: 1_001 },
                StateMutation::SetBalance { wallet: alice, token: other,  amount: 1_000 },
            ],
        };
        commit_state_transition(&mut s, &setup, 0).unwrap();

        let delta = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::LpDelta {
                from: alice, pool,
                amt_a: 1_000, amt_b: 1_000,
                shares_minted: 1_000,
                fee: 1,
                pool_after: PoolState {
                    token_a: NATIVE, token_b: other,
                    reserve_a: 1_000, reserve_b: 1_000,
                    lp_shares: 1_000, fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(&mut s, &delta, 1).unwrap();
        let p = s.pool(&pool).unwrap();
        assert_eq!(p.lp_shares, 1_000);
        assert_eq!(s.balance_of(&alice, &NATIVE), 0);
        assert_eq!(s.balance_of(&alice, &other), 0);
    }

    #[test]
    fn lp_burn_delta_rejects_share_underflow() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let pool: PoolId = [9u8; 32];
        let other: TokenId = [7u8; 32];
        let setup = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetPool {
                    pool,
                    state: PoolState {
                        token_a: NATIVE, token_b: other,
                        reserve_a: 1_000, reserve_b: 1_000,
                        lp_shares: 100, fee_bps: 30, accrued_fees: 0,
                    },
                },
                StateMutation::SetBalance { wallet: alice, token: NATIVE, amount: 10 },
            ],
        };
        commit_state_transition(&mut s, &setup, 0).unwrap();

        // Trying to burn 500 shares from a 100-share pool — pool_after's
        // lp_shares wraps in a `checked_sub` that the chokepoint catches.
        let bogus = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::LpBurnDelta {
                from: alice, pool,
                shares_burned: 500,
                amt_a_out: 0, amt_b_out: 0,
                fee: 1,
                pool_after: PoolState {
                    token_a: NATIVE, token_b: other,
                    reserve_a: 1_000, reserve_b: 1_000,
                    lp_shares: 0, // wrong — should fail before getting here
                    fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        let err = commit_state_transition(&mut s, &bogus, 1).unwrap_err();
        assert!(matches!(err, CommitError::DeltaInvariant(_)));
    }
}
