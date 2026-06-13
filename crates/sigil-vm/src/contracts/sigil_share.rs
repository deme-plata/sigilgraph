//! SIGIL Treasury Share (SSHARE) — NAV-backed equity token.
//!
//! Port of Quillon's `q-vm/src/contracts/qshare_token.rs` (QSHARE, the L3
//! layer of the 3-layer capital stack) to SIGIL, per LANE-W. Autonomous
//! on-chain premium-arbitrage contract: mints new shares when the
//! SSHARE/SIGIL market price exceeds NAV (premium), buys back + burns when
//! below (discount). Result: SIGIL-per-share rises reflexively without human
//! market-timing.
//!
//! ## Deliberate divergences from the QSHARE original (the LANE-W spec)
//!
//! 1. **Decimals 8, not 24.** SIGIL convention (`sigil_state::SIGIL_DECIMALS`).
//!    All base-unit constants are scaled accordingly.
//! 2. **Treasury = the sigil-bank pool.** QSHARE held a basket of QCREDIT
//!    vault positions inside its own struct. SSHARE's treasury is the
//!    [`SSHARE_TREASURY`] *wallet* — a bank-pool wallet in the committed
//!    wallet SMT, whose outbound spends are council-gated via the
//!    commons-rails M-of-N (`sigil-treasury` payout gate). The
//!    [`TreasuryPosition`] basket is preserved (LANE-X QCREDIT integration
//!    target) but committed as contract slots, not a side struct.
//! 3. **NO separate mutable supply-state** (the Quillon-postmortem rule).
//!    QSHARE kept `circulating_qshare: u128` as a mutable field on a struct
//!    persisted outside the roots. Here the circulating supply lives in a
//!    contract storage slot ([`SLOT_SUPPLY`]) committed in
//!    `contract_state_root`, and every change to it goes through
//!    `sigil_state::commit_state_transition` in the same atomic transition
//!    as the balance/pool mutations it accounts for. There is no way to
//!    move supply without moving the root.
//! 4. **Thin-pool guard KEPT.** [`DexPoolSnapshot::is_sufficiently_deep`]
//!    refuses mint AND buyback against a shallow pool — the manipulation
//!    guard the spec explicitly pins.
//!
//! Events are emitted into the block's `event_log_root` via
//! `StateMutation::PushEventHash` with domain-separated BLAKE3 hashes.

use serde::{Deserialize, Serialize};

use sigil_state::{
    commit_state_transition, u128_str, CommitError, ContractId, PoolState, SigilState, SlotId,
    StateMutation, StateTransition, TokenId, WalletId, NATIVE,
};

// ============ PROTOCOL CONSTANTS ============

/// SSHARE decimals — SIGIL convention (8), NOT QUG's 24.
pub const SSHARE_DECIMALS: u32 = 8;

/// One whole SSHARE in base units.
pub const ONE_SSHARE: u128 = 10u128.pow(SSHARE_DECIMALS);

// Compile-time correctness: decimals stay in lockstep with the native token.
const _: () = assert!(SSHARE_DECIMALS == sigil_state::SIGIL_DECIMALS, "SSHARE must match SIGIL decimals");
const _: () = assert!(ONE_SSHARE == 100_000_000, "SSHARE base unit must be 10^8");

/// SSHARE token id in the wallet SMT. (USDS is `[0xD5; 32]`, QCREDIT is
/// `[0xC1; 32]` — this must stay distinct from every other token sentinel.)
pub const SSHARE: TokenId = [0x55; 32];

/// SSHARE symbol.
pub const SSHARE_SYMBOL: &str = "SSHARE";

/// The contract id under which all SSHARE slots live in contract storage.
pub const SSHARE_CONTRACT: ContractId = [0x5C; 32];

/// The treasury wallet — the sigil-bank pool backing SSHARE's NAV. Inbound:
/// mint accumulation. Outbound: ONLY through buyback (here) or the council
/// M-of-N payout gate (`sigil-treasury`) — never a master key.
pub const SSHARE_TREASURY: WalletId = [0xB5; 32];

/// The position vault — principal locked out of the pending pool into
/// [`TreasuryPosition`]s parks here (LANE-X QCREDIT vault integration point).
pub const SSHARE_POSITION_VAULT: WalletId = [0xB6; 32];

/// Premium ratio (×1000) above which mint is allowed.
/// 1500 = market price must be ≥1.5× NAV to trigger.
pub const MINT_THRESHOLD_BPS: u64 = 1500;

/// Premium ratio (×1000) below which buyback is allowed.
/// 950 = market price must be ≤0.95× NAV to trigger.
pub const DISCOUNT_THRESHOLD_BPS: u64 = 950;

/// Minimum blocks between consecutive mint events.
pub const MINT_COOLDOWN_BLOCKS: u64 = 360;

/// Minimum blocks between consecutive buyback events. Tighter than mint.
pub const BUYBACK_COOLDOWN_BLOCKS: u64 = 720;

/// Max SSHARE minted per trigger as a fraction of DEX pool depth (bps of
/// SIGIL reserves). 50 = 0.5%. Prevents self-bidding the SSHARE price.
pub const MAX_POOL_FRACTION_BPS: u64 = 50;

/// Max SSHARE inflation per mint as bps of circulating supply. 200 = 2%.
/// Defense in depth behind the pool-fraction cap.
pub const MAX_INFLATION_PER_MINT_BPS: u64 = 200;

/// Max buyback per trigger as fraction of pool depth (bps). 10 = 0.1% —
/// tighter than mint to prevent gaming the buyback path.
pub const MAX_BUYBACK_POOL_FRACTION_BPS: u64 = 10;

/// Min DEX pool depth (SIGIL base units, 8 decimals) before any mint or
/// buyback is allowed. **The thin-pool manipulation guard — KEEP.**
/// 1000 SIGIL.
pub const MIN_POOL_DEPTH_SIGIL: u128 = 1_000 * ONE_SSHARE;

/// Mint/buyback trigger fee. 0.01 SIGIL (8 decimals). The tx layer escrows
/// it; refunded + bounty on success, kept on failure (spam gate).
pub const MINT_TRIGGER_FEE: u128 = ONE_SSHARE / 100;

/// Bounty to the caller of a successful mint, as bps of accumulated SIGIL.
pub const MINT_BOUNTY_BPS: u64 = 50;

/// Absolute cap on the mint bounty: 1 SIGIL.
pub const MAX_MINT_BOUNTY: u128 = ONE_SSHARE;

/// Seconds in a year, for yield accrual on treasury positions.
pub const SECONDS_PER_YEAR: u64 = 365 * 24 * 3600;

// ============ CONTRACT STORAGE SLOTS ============
//
// All SSHARE state is committed in `contract_state_root` under
// `SSHARE_CONTRACT`. Slot ids are tagged constants; values are fixed-layout
// 32-byte encodings. The ONLY writer is `commit_state_transition` (the
// `SetContractSlot` mutations built below) — rule #6, by construction.

const fn slot_tag(tag: u8) -> SlotId {
    let mut s = [0u8; 32];
    s[0] = 0x55; // 'U' — SSHARE namespace
    s[1] = tag;
    s
}

/// Circulating SSHARE supply (u128 LE in bytes 0..16). THE supply record —
/// no other supply state exists anywhere.
pub const SLOT_SUPPLY: SlotId = slot_tag(1);
/// Height of the most recent successful mint (u64 LE in bytes 0..8).
pub const SLOT_LAST_MINT_HEIGHT: SlotId = slot_tag(2);
/// Height of the most recent successful buyback.
pub const SLOT_LAST_BUYBACK_HEIGHT: SlotId = slot_tag(3);
/// Number of treasury positions (u64 LE).
pub const SLOT_POSITION_COUNT: SlotId = slot_tag(4);
/// Cached NAV oracle: nav_per_sshare u128 ‖ height u64 ‖ timestamp u64.
pub const SLOT_NAV_ORACLE: SlotId = slot_tag(5);
/// Lifetime counters: mints u64 ‖ buybacks u64.
pub const SLOT_LIFETIME_OPS: SlotId = slot_tag(6);
/// Lifetime SIGIL accumulated into the treasury by mints (u128 LE).
pub const SLOT_LIFETIME_ACCUMULATED: SlotId = slot_tag(7);
/// Lifetime SSHARE burned by buybacks (u128 LE).
pub const SLOT_LIFETIME_BURNED: SlotId = slot_tag(8);

/// Slot for treasury position `index` — BLAKE3 domain-separated so position
/// slots can never collide with the tagged constants above.
pub fn position_slot(index: u64) -> SlotId {
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/sshare/pos/v1");
    h.update(&index.to_le_bytes());
    *h.finalize().as_bytes()
}

fn enc_u128(v: u128) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[0..16].copy_from_slice(&v.to_le_bytes());
    s
}
fn dec_u128(s: &[u8; 32]) -> u128 {
    u128::from_le_bytes(s[0..16].try_into().unwrap())
}
fn enc_u64(v: u64) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[0..8].copy_from_slice(&v.to_le_bytes());
    s
}
fn dec_u64(s: &[u8; 32]) -> u64 {
    u64::from_le_bytes(s[0..8].try_into().unwrap())
}

// ============ CORE DATA STRUCTURES ============

/// A single treasury position: principal locked out of the pending pool,
/// accruing linear yield at `yield_bps` APY. QSHARE's `CreditTier` enum is
/// generalized to a bps field — LANE-X's QCREDIT port plugs its tiers in
/// here (`tier.apy_bps()` → `yield_bps`).
///
/// Committed encoding (32 bytes): principal u128 ‖ yield_bps u32 ‖
/// locked_at_timestamp u64 ‖ 4 zero bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreasuryPosition {
    /// SIGIL locked when this position was opened (base units, 8 decimals).
    #[serde(with = "u128_str")]
    pub principal: u128,
    /// Linear APY in basis points (2500 = 25%).
    pub yield_bps: u32,
    /// Unix-seconds timestamp when locked / last harvested, for accrual.
    pub locked_at_timestamp: u64,
}

impl TreasuryPosition {
    /// Accrued value (principal + yield) at `now_ts`. Linear accrual,
    /// identical math to the QSHARE original.
    pub fn value_at(&self, now_ts: u64) -> u128 {
        if now_ts <= self.locked_at_timestamp {
            return self.principal;
        }
        let elapsed_s = now_ts - self.locked_at_timestamp;
        let scaled = self.principal.saturating_mul(self.yield_bps as u128);
        let yield_amount = scaled
            .saturating_mul(elapsed_s as u128)
            .checked_div(10_000u128 * SECONDS_PER_YEAR as u128)
            .unwrap_or(0);
        self.principal.saturating_add(yield_amount)
    }

    fn encode(&self) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0..16].copy_from_slice(&self.principal.to_le_bytes());
        s[16..20].copy_from_slice(&self.yield_bps.to_le_bytes());
        s[20..28].copy_from_slice(&self.locked_at_timestamp.to_le_bytes());
        s
    }

    fn decode(s: &[u8; 32]) -> Self {
        Self {
            principal: u128::from_le_bytes(s[0..16].try_into().unwrap()),
            yield_bps: u32::from_le_bytes(s[16..20].try_into().unwrap()),
            locked_at_timestamp: u64::from_le_bytes(s[20..28].try_into().unwrap()),
        }
    }
}

/// Treasury composition snapshot — the agentic-interface view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreasuryComposition {
    /// Liquid SIGIL in the [`SSHARE_TREASURY`] bank-pool wallet.
    #[serde(with = "u128_str")]
    pub pending_sigil: u128,
    /// Locked positions (LANE-X vault basket).
    pub positions: Vec<TreasuryPosition>,
    /// Total SIGIL-equivalent treasury value at the snapshot timestamp.
    #[serde(with = "u128_str")]
    pub total_nav_sigil_equivalent: u128,
    /// SSHARE in circulation (the committed [`SLOT_SUPPLY`] value).
    #[serde(with = "u128_str")]
    pub circulating_sshare: u128,
    /// NAV per SSHARE (×[`ONE_SSHARE`] fixed-point).
    #[serde(with = "u128_str")]
    pub nav_per_sshare: u128,
    /// Block height of the snapshot.
    pub computed_at_height: u64,
    /// Unix-seconds timestamp of the snapshot.
    pub computed_at_timestamp: u64,
}

/// Cached NAV state — refreshed on every mint/buyback, committed in
/// [`SLOT_NAV_ORACLE`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavOracleState {
    /// NAV per SSHARE at last refresh (×[`ONE_SSHARE`] fixed-point).
    #[serde(with = "u128_str")]
    pub last_nav_per_sshare: u128,
    /// Height of last refresh.
    pub last_computed_height: u64,
    /// Timestamp of last refresh.
    pub last_computed_timestamp: u64,
}

impl NavOracleState {
    fn encode(&self) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0..16].copy_from_slice(&self.last_nav_per_sshare.to_le_bytes());
        s[16..24].copy_from_slice(&self.last_computed_height.to_le_bytes());
        s[24..32].copy_from_slice(&self.last_computed_timestamp.to_le_bytes());
        s
    }
    fn decode(s: &[u8; 32]) -> Self {
        Self {
            last_nav_per_sshare: u128::from_le_bytes(s[0..16].try_into().unwrap()),
            last_computed_height: u64::from_le_bytes(s[16..24].try_into().unwrap()),
            last_computed_timestamp: u64::from_le_bytes(s[24..32].try_into().unwrap()),
        }
    }
}

/// DEX pool snapshot for mint/buyback decisions — SIGIL/SSHARE pair.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DexPoolSnapshot {
    /// NATIVE SIGIL reserves in the SSHARE/SIGIL pool (base units).
    #[serde(with = "u128_str")]
    pub sigil_reserves: u128,
    /// SSHARE reserves in the same pool.
    #[serde(with = "u128_str")]
    pub sshare_reserves: u128,
    /// Time-weighted average price of SSHARE in SIGIL (×[`ONE_SSHARE`]
    /// fixed-point), supplied by the caller's TWAP window.
    #[serde(with = "u128_str")]
    pub twap_sigil_per_sshare: u128,
}

impl DexPoolSnapshot {
    /// **The thin-pool guard.** True iff the pool is deep enough to allow
    /// mint/buyback. KEPT verbatim from QSHARE per the LANE-W spec — a
    /// shallow pool is trivially manipulable into a fake premium/discount.
    pub fn is_sufficiently_deep(&self) -> bool {
        self.sigil_reserves >= MIN_POOL_DEPTH_SIGIL && self.sshare_reserves > 0
    }

    /// Build a snapshot from a committed [`PoolState`], mapping whichever
    /// side holds NATIVE vs SSHARE. Returns `None` if the pool is not the
    /// SIGIL/SSHARE pair.
    pub fn from_pool(pool: &PoolState, twap_sigil_per_sshare: u128) -> Option<Self> {
        let (sigil_reserves, sshare_reserves) = if pool.token_a == NATIVE && pool.token_b == SSHARE
        {
            (pool.reserve_a, pool.reserve_b)
        } else if pool.token_a == SSHARE && pool.token_b == NATIVE {
            (pool.reserve_b, pool.reserve_a)
        } else {
            return None;
        };
        Some(Self { sigil_reserves, sshare_reserves, twap_sigil_per_sshare })
    }

    /// Spot price (SIGIL per SSHARE, ×[`ONE_SSHARE`]) from raw reserves —
    /// the TWAP fallback for tests/bootstrap. Production callers should
    /// supply a real TWAP.
    pub fn spot_price(sigil_reserves: u128, sshare_reserves: u128) -> u128 {
        if sshare_reserves == 0 {
            return 0;
        }
        sigil_reserves
            .saturating_mul(ONE_SSHARE)
            .checked_div(sshare_reserves)
            .unwrap_or(0)
    }
}

// ============ EVENTS ============

/// Event emitted (into `event_log_root`) on successful mint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareMintEvent {
    /// SSHARE minted into the pool.
    #[serde(with = "u128_str")]
    pub minted_sshare: u128,
    /// SIGIL accumulated into the treasury (net of bounty).
    #[serde(with = "u128_str")]
    pub sigil_accumulated: u128,
    /// NAV per SSHARE after the mint.
    #[serde(with = "u128_str")]
    pub new_nav_per_sshare: u128,
    /// Premium ratio at trigger ×1000.
    pub premium_ratio_bps: u64,
    /// Caller that triggered the mint (receives the bounty).
    pub trigger_caller: WalletId,
    /// Bounty paid to the caller.
    #[serde(with = "u128_str")]
    pub bounty_paid_sigil: u128,
    /// Block height of the mint.
    pub block_height: u64,
}

impl ShareMintEvent {
    /// Domain-separated BLAKE3 hash for `event_log_root` inclusion.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"SIGIL/sshare/mint/v1");
        h.update(&self.minted_sshare.to_le_bytes());
        h.update(&self.sigil_accumulated.to_le_bytes());
        h.update(&self.new_nav_per_sshare.to_le_bytes());
        h.update(&self.premium_ratio_bps.to_le_bytes());
        h.update(&self.trigger_caller);
        h.update(&self.bounty_paid_sigil.to_le_bytes());
        h.update(&self.block_height.to_le_bytes());
        *h.finalize().as_bytes()
    }
}

/// Event emitted on successful buyback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareBuybackEvent {
    /// SSHARE bought back from the pool and burned.
    #[serde(with = "u128_str")]
    pub burned_sshare: u128,
    /// SIGIL spent into the pool.
    #[serde(with = "u128_str")]
    pub sigil_spent: u128,
    /// NAV per SSHARE after the burn.
    #[serde(with = "u128_str")]
    pub new_nav_per_sshare: u128,
    /// Discount ratio at trigger ×1000 (e.g. 920 = 0.92× NAV).
    pub discount_ratio_bps: u64,
    /// Caller that triggered the buyback.
    pub trigger_caller: WalletId,
    /// Bounty paid to the caller.
    #[serde(with = "u128_str")]
    pub bounty_paid_sigil: u128,
    /// Block height of the buyback.
    pub block_height: u64,
}

impl ShareBuybackEvent {
    /// Domain-separated BLAKE3 hash for `event_log_root` inclusion.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"SIGIL/sshare/buyback/v1");
        h.update(&self.burned_sshare.to_le_bytes());
        h.update(&self.sigil_spent.to_le_bytes());
        h.update(&self.new_nav_per_sshare.to_le_bytes());
        h.update(&self.discount_ratio_bps.to_le_bytes());
        h.update(&self.trigger_caller);
        h.update(&self.bounty_paid_sigil.to_le_bytes());
        h.update(&self.block_height.to_le_bytes());
        *h.finalize().as_bytes()
    }
}

/// Result of a successful mint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintResult {
    /// The committed event.
    pub event: ShareMintEvent,
    /// Bounty credited to the caller.
    #[serde(with = "u128_str")]
    pub bounty_paid_to_caller: u128,
    /// Post-mint circulating supply (the new [`SLOT_SUPPLY`] value).
    #[serde(with = "u128_str")]
    pub new_circulating_sshare: u128,
}

/// Result of a successful buyback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuybackResult {
    /// The committed event.
    pub event: ShareBuybackEvent,
    /// Bounty credited to the caller.
    #[serde(with = "u128_str")]
    pub bounty_paid_to_caller: u128,
    /// Post-burn circulating supply.
    #[serde(with = "u128_str")]
    pub new_circulating_sshare: u128,
}

// ============ ERRORS ============

/// Reasons a mint attempt fails. The transition is NOT committed on error.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    /// Market premium below the 1.5× NAV threshold.
    #[error("premium {current_bps}<{required_bps} (×1000)")]
    PremiumBelowThreshold {
        /// Observed premium ratio ×1000.
        current_bps: u64,
        /// Required threshold ×1000.
        required_bps: u64,
    },
    /// Mint cooldown window still open.
    #[error("cooldown: height {current_height} < eligible {eligible_at}")]
    CooldownActive {
        /// Current block height.
        current_height: u64,
        /// First eligible height.
        eligible_at: u64,
    },
    /// **Thin-pool guard tripped** — pool below [`MIN_POOL_DEPTH_SIGIL`].
    #[error("pool too shallow: {current_sigil_reserves} < {required}")]
    PoolTooShallow {
        /// Observed SIGIL reserves.
        current_sigil_reserves: u128,
        /// Required minimum depth.
        required: u128,
    },
    /// Trigger fee below [`MINT_TRIGGER_FEE`].
    #[error("trigger fee {paid} < required {required}")]
    TriggerFeeInsufficient {
        /// Fee escrowed by the caller.
        paid: u128,
        /// Required fee.
        required: u128,
    },
    /// NAV unavailable (zero supply or zero TWAP).
    #[error("nav oracle stale (zero supply or zero twap)")]
    NavOracleStale,
    /// Arithmetic overflow / degenerate math.
    #[error("computation overflow")]
    ComputationOverflow,
    /// The referenced pool doesn't exist or isn't the SIGIL/SSHARE pair.
    #[error("pool is not the SIGIL/SSHARE pair (or unknown)")]
    PoolPairMismatch,
    /// SSHARE not bootstrapped yet (supply slot is zero).
    #[error("SSHARE not bootstrapped")]
    NotBootstrapped,
    /// Chokepoint rejected the transition.
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Reasons a buyback attempt fails.
#[derive(Debug, thiserror::Error)]
pub enum BuybackError {
    /// Market price above the 0.95× NAV discount threshold.
    #[error("discount {current_bps}>{required_bps} (×1000)")]
    DiscountAboveThreshold {
        /// Observed ratio ×1000.
        current_bps: u64,
        /// Required threshold ×1000.
        required_bps: u64,
    },
    /// Buyback cooldown window still open.
    #[error("cooldown: height {current_height} < eligible {eligible_at}")]
    CooldownActive {
        /// Current block height.
        current_height: u64,
        /// First eligible height.
        eligible_at: u64,
    },
    /// **Thin-pool guard tripped.**
    #[error("pool too shallow: {current_sigil_reserves} < {required}")]
    PoolTooShallow {
        /// Observed SIGIL reserves.
        current_sigil_reserves: u128,
        /// Required minimum depth.
        required: u128,
    },
    /// Trigger fee below [`MINT_TRIGGER_FEE`].
    #[error("trigger fee {paid} < required {required}")]
    TriggerFeeInsufficient {
        /// Fee escrowed by the caller.
        paid: u128,
        /// Required fee.
        required: u128,
    },
    /// No accrued yield to spend — buyback NEVER spends principal.
    #[error("insufficient yield: available {available}, needed {needed}")]
    InsufficientYield {
        /// Yield available.
        available: u128,
        /// Minimum needed.
        needed: u128,
    },
    /// NAV unavailable.
    #[error("nav oracle stale")]
    NavOracleStale,
    /// The referenced pool doesn't exist or isn't the SIGIL/SSHARE pair.
    #[error("pool is not the SIGIL/SSHARE pair (or unknown)")]
    PoolPairMismatch,
    /// Arithmetic overflow / degenerate math.
    #[error("computation overflow")]
    ComputationOverflow,
    /// Chokepoint rejected the transition.
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

/// Errors from bootstrap / position recording.
#[derive(Debug, thiserror::Error)]
pub enum ShareAdminError {
    /// Bootstrap may only run once (supply slot must be zero).
    #[error("SSHARE already bootstrapped")]
    AlreadyBootstrapped,
    /// Funder/treasury balance can't cover the requested amount.
    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance {
        /// Balance available.
        have: u128,
        /// Amount required.
        need: u128,
    },
    /// Zero amount supplied.
    #[error("zero amount")]
    ZeroAmount,
    /// Chokepoint rejected the transition.
    #[error("commit: {0}")]
    Commit(#[from] CommitError),
}

// ============ READ-ONLY VIEWS ============

/// Circulating SSHARE supply — read from the committed [`SLOT_SUPPLY`].
pub fn circulating_sshare(state: &SigilState) -> u128 {
    dec_u128(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_SUPPLY))
}

/// Number of committed treasury positions.
pub fn position_count(state: &SigilState) -> u64 {
    dec_u64(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_POSITION_COUNT))
}

/// All committed treasury positions, in append order.
pub fn treasury_positions(state: &SigilState) -> Vec<TreasuryPosition> {
    (0..position_count(state))
        .map(|i| TreasuryPosition::decode(&state.contract_slot(&SSHARE_CONTRACT, &position_slot(i))))
        .collect()
}

/// Total SIGIL-equivalent treasury value at `now_ts`: the liquid bank-pool
/// wallet + every position's accrued value.
pub fn nav_total_sigil_equivalent(state: &SigilState, now_ts: u64) -> u128 {
    let mut total = state.balance_of(&SSHARE_TREASURY, &NATIVE);
    for pos in treasury_positions(state) {
        total = total.saturating_add(pos.value_at(now_ts));
    }
    total
}

/// NAV per SSHARE (×[`ONE_SSHARE`] fixed-point). 0 if nothing circulates.
pub fn nav_per_sshare(state: &SigilState, now_ts: u64) -> u128 {
    let circ = circulating_sshare(state);
    if circ == 0 {
        return 0;
    }
    let total = nav_total_sigil_equivalent(state, now_ts);
    total
        .checked_mul(ONE_SSHARE)
        .unwrap_or(u128::MAX)
        / circ
}

/// Premium ratio ×1000 (market TWAP / NAV). `None` if NAV or TWAP is zero.
pub fn premium_ratio_bps(state: &SigilState, pool: &DexPoolSnapshot, now_ts: u64) -> Option<u64> {
    let nav = nav_per_sshare(state, now_ts);
    if nav == 0 || pool.twap_sigil_per_sshare == 0 {
        return None;
    }
    let ratio = pool.twap_sigil_per_sshare.checked_mul(1000)?.checked_div(nav)?;
    Some(ratio.min(u64::MAX as u128) as u64)
}

/// First height at which the next mint becomes eligible.
pub fn next_mint_eligible_at_height(state: &SigilState) -> u64 {
    dec_u64(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_LAST_MINT_HEIGHT))
        .saturating_add(MINT_COOLDOWN_BLOCKS)
}

/// First height at which the next buyback becomes eligible.
pub fn next_buyback_eligible_at_height(state: &SigilState) -> u64 {
    dec_u64(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_LAST_BUYBACK_HEIGHT))
        .saturating_add(BUYBACK_COOLDOWN_BLOCKS)
}

/// Cached NAV oracle (committed [`SLOT_NAV_ORACLE`]).
pub fn nav_oracle(state: &SigilState) -> NavOracleState {
    NavOracleState::decode(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_NAV_ORACLE))
}

/// Treasury composition snapshot for the agentic interface.
pub fn treasury_composition(state: &SigilState, height: u64, now_ts: u64) -> TreasuryComposition {
    TreasuryComposition {
        pending_sigil: state.balance_of(&SSHARE_TREASURY, &NATIVE),
        positions: treasury_positions(state),
        total_nav_sigil_equivalent: nav_total_sigil_equivalent(state, now_ts),
        circulating_sshare: circulating_sshare(state),
        nav_per_sshare: nav_per_sshare(state, now_ts),
        computed_at_height: height,
        computed_at_timestamp: now_ts,
    }
}

/// [`sigil_dex::registry::TokenMetadata`] for SSHARE — the registry row the
/// LANE-W spec requires. Call after [`bootstrap`] and feed to
/// `TokenRegistry::register_token`.
pub fn sshare_metadata(created_at_height: u64, total_supply: u128) -> sigil_dex::registry::TokenMetadata {
    sigil_dex::registry::TokenMetadata {
        token_id: SSHARE,
        symbol: SSHARE_SYMBOL.to_string(),
        name: "SIGIL Treasury Share".to_string(),
        decimals: SSHARE_DECIMALS as u8,
        total_supply,
        creator: SSHARE_TREASURY,
        created_at_height,
        is_active: true,
        has_liquidity_pool: false,
        liquidity_pools: Vec::new(),
        description: Some("NAV-backed treasury share — autonomous premium-arbitrage mint/buyback (QSHARE port)".to_string()),
        website: None,
    }
}

// ============ MINT MECHANISM ============

/// Max SSHARE mintable in one trigger — double-capped by pool fraction
/// (0.5% of SIGIL reserves, expressed in SSHARE at the pool ratio) and
/// inflation rate (2% of circulating). Verbatim QSHARE math.
fn compute_max_mint_amount(pool: &DexPoolSnapshot, circulating: u128) -> u128 {
    let pool_sigil_cap = pool
        .sigil_reserves
        .saturating_mul(MAX_POOL_FRACTION_BPS as u128)
        / 10_000;
    let cap_via_pool = if pool.sigil_reserves == 0 {
        0
    } else {
        pool_sigil_cap.saturating_mul(pool.sshare_reserves) / pool.sigil_reserves
    };
    let cap_via_inflation = circulating
        .saturating_mul(MAX_INFLATION_PER_MINT_BPS as u128)
        / 10_000;
    cap_via_pool.min(cap_via_inflation)
}

/// Build `pool_after` with the SSHARE side shifted by `+sshare_delta` and
/// the SIGIL side by `sigil_delta` (negative = drained). Returns `None` on
/// underflow.
fn shift_pool(
    pool: &PoolState,
    sshare_add: u128,
    sigil_sub: u128,
    sigil_add: u128,
    sshare_sub: u128,
) -> Option<PoolState> {
    let mut after = pool.clone();
    if pool.token_a == NATIVE {
        after.reserve_a = after.reserve_a.checked_add(sigil_add)?.checked_sub(sigil_sub)?;
        after.reserve_b = after.reserve_b.checked_add(sshare_add)?.checked_sub(sshare_sub)?;
    } else {
        after.reserve_a = after.reserve_a.checked_add(sshare_add)?.checked_sub(sshare_sub)?;
        after.reserve_b = after.reserve_b.checked_add(sigil_add)?.checked_sub(sigil_sub)?;
    }
    Some(after)
}

/// Permissionless mint trigger.
///
/// When SSHARE trades ≥1.5× NAV, anyone may call this: the contract mints a
/// double-capped amount of SSHARE INTO the pool (selling it at the pool
/// price), the SIGIL proceeds accumulate in the [`SSHARE_TREASURY`]
/// bank-pool wallet (raising NAV), and the caller earns a bounty. ALL
/// effects — pool shift, treasury credit, bounty, supply-slot bump, NAV
/// oracle refresh, lifetime stats, event hash — are ONE atomic
/// `StateTransition` through `commit_state_transition`.
///
/// `trigger_fee_paid` is the fee the tx layer escrowed from the caller; it
/// gates spam (refunded on success, kept by the tx layer on failure).
#[allow(clippy::too_many_arguments)]
pub fn try_autonomous_mint(
    state: &mut SigilState,
    height: u64,
    now_ts: u64,
    caller: WalletId,
    trigger_fee_paid: u128,
    pool_id: sigil_state::PoolId,
    twap_sigil_per_sshare: u128,
) -> Result<MintResult, MintError> {
    // Gate 1: trigger fee.
    if trigger_fee_paid < MINT_TRIGGER_FEE {
        return Err(MintError::TriggerFeeInsufficient {
            paid: trigger_fee_paid,
            required: MINT_TRIGGER_FEE,
        });
    }
    // Gate 2: cooldown.
    let eligible_at = next_mint_eligible_at_height(state);
    if dec_u64(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_LAST_MINT_HEIGHT)) != 0
        && height < eligible_at
    {
        return Err(MintError::CooldownActive { current_height: height, eligible_at });
    }
    // Pool lookup + pair mapping.
    let pool_state = state.pool(&pool_id).cloned().ok_or(MintError::PoolPairMismatch)?;
    let pool = DexPoolSnapshot::from_pool(&pool_state, twap_sigil_per_sshare)
        .ok_or(MintError::PoolPairMismatch)?;
    // Gate 3: THE thin-pool guard (kept).
    if !pool.is_sufficiently_deep() {
        return Err(MintError::PoolTooShallow {
            current_sigil_reserves: pool.sigil_reserves,
            required: MIN_POOL_DEPTH_SIGIL,
        });
    }
    let circ = circulating_sshare(state);
    if circ == 0 {
        return Err(MintError::NotBootstrapped);
    }
    // Gate 4: premium threshold — on BOTH the TWAP and the live spot price.
    // (Hardening over the QSHARE original, which gated on TWAP alone: the
    // mint EXECUTES against the spot reserves, so a TWAP/spot divergence —
    // e.g. a flash-moved pool — could pass the TWAP gate yet sell shares
    // below NAV. Found by the LANE-W chronos gate.)
    let premium = premium_ratio_bps(state, &pool, now_ts).ok_or(MintError::NavOracleStale)?;
    if premium < MINT_THRESHOLD_BPS {
        return Err(MintError::PremiumBelowThreshold {
            current_bps: premium,
            required_bps: MINT_THRESHOLD_BPS,
        });
    }
    let spot = DexPoolSnapshot::spot_price(pool.sigil_reserves, pool.sshare_reserves);
    let spot_pool = DexPoolSnapshot { twap_sigil_per_sshare: spot, ..pool };
    let spot_premium =
        premium_ratio_bps(state, &spot_pool, now_ts).ok_or(MintError::NavOracleStale)?;
    if spot_premium < MINT_THRESHOLD_BPS {
        return Err(MintError::PremiumBelowThreshold {
            current_bps: spot_premium,
            required_bps: MINT_THRESHOLD_BPS,
        });
    }

    // Mint sizing (double-capped).
    let mint_amount = compute_max_mint_amount(&pool, circ);
    if mint_amount == 0 {
        return Err(MintError::ComputationOverflow);
    }

    // Constant-product swap simulation: sell mint_amount SSHARE into the pool.
    // sigil_out = sigil_reserves × mint_amount / (sshare_reserves + mint_amount)
    let new_sshare_reserves = pool
        .sshare_reserves
        .checked_add(mint_amount)
        .ok_or(MintError::ComputationOverflow)?;
    let sigil_out = pool
        .sigil_reserves
        .checked_mul(mint_amount)
        .ok_or(MintError::ComputationOverflow)?
        .checked_div(new_sshare_reserves)
        .ok_or(MintError::ComputationOverflow)?;

    let bounty = (sigil_out.saturating_mul(MINT_BOUNTY_BPS as u128) / 10_000).min(MAX_MINT_BOUNTY);
    let sigil_to_treasury = sigil_out.saturating_sub(bounty);

    let pool_after = shift_pool(&pool_state, mint_amount, sigil_out, 0, 0)
        .ok_or(MintError::ComputationOverflow)?;

    // Post-state values, computed analytically so the event can carry them.
    let new_circ = circ.checked_add(mint_amount).ok_or(MintError::ComputationOverflow)?;
    let new_total = nav_total_sigil_equivalent(state, now_ts).saturating_add(sigil_to_treasury);
    let new_nav = new_total.checked_mul(ONE_SSHARE).unwrap_or(u128::MAX) / new_circ;

    let event = ShareMintEvent {
        minted_sshare: mint_amount,
        sigil_accumulated: sigil_to_treasury,
        new_nav_per_sshare: new_nav,
        premium_ratio_bps: premium,
        trigger_caller: caller,
        bounty_paid_sigil: bounty,
        block_height: height,
    };

    // Lifetime stats.
    let ops = state.contract_slot(&SSHARE_CONTRACT, &SLOT_LIFETIME_OPS);
    let mints = u64::from_le_bytes(ops[0..8].try_into().unwrap()).saturating_add(1);
    let buybacks = u64::from_le_bytes(ops[8..16].try_into().unwrap());
    let mut ops_enc = [0u8; 32];
    ops_enc[0..8].copy_from_slice(&mints.to_le_bytes());
    ops_enc[8..16].copy_from_slice(&buybacks.to_le_bytes());
    let accumulated = dec_u128(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_LIFETIME_ACCUMULATED))
        .saturating_add(sigil_to_treasury);

    // Balance mutations — merge if caller IS the treasury (degenerate but legal).
    let mut mutations: Vec<StateMutation> = Vec::with_capacity(10);
    if caller == SSHARE_TREASURY {
        let bal = state.balance_of(&SSHARE_TREASURY, &NATIVE);
        mutations.push(StateMutation::SetBalance {
            wallet: SSHARE_TREASURY,
            token: NATIVE,
            amount: bal.saturating_add(sigil_to_treasury).saturating_add(bounty),
        });
    } else {
        let tbal = state.balance_of(&SSHARE_TREASURY, &NATIVE);
        let cbal = state.balance_of(&caller, &NATIVE);
        mutations.push(StateMutation::SetBalance {
            wallet: SSHARE_TREASURY,
            token: NATIVE,
            amount: tbal.saturating_add(sigil_to_treasury),
        });
        mutations.push(StateMutation::SetBalance {
            wallet: caller,
            token: NATIVE,
            amount: cbal.saturating_add(bounty),
        });
    }
    mutations.push(StateMutation::SetPool { pool: pool_id, state: pool_after });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_SUPPLY,
        value: enc_u128(new_circ),
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_LAST_MINT_HEIGHT,
        value: enc_u64(height),
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_NAV_ORACLE,
        value: NavOracleState {
            last_nav_per_sshare: new_nav,
            last_computed_height: height,
            last_computed_timestamp: now_ts,
        }
        .encode(),
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_LIFETIME_OPS,
        value: ops_enc,
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_LIFETIME_ACCUMULATED,
        value: enc_u128(accumulated),
    });
    mutations.push(StateMutation::PushEventHash(event.hash()));

    let t = StateTransition { at_height: height, mutations };
    commit_state_transition(state, &t, height)?;

    Ok(MintResult {
        event,
        bounty_paid_to_caller: bounty,
        new_circulating_sshare: new_circ,
    })
}

/// Permissionless buyback trigger — symmetric to mint.
///
/// When SSHARE trades ≤0.95× NAV, anyone may trigger: accrued treasury
/// *yield* (NEVER principal) is realized from the position basket, spent
/// into the pool buying SSHARE, and the bought SSHARE is burned (supply
/// slot decreases). Position accrual baselines reset to `now_ts` on
/// harvest. One atomic transition through the chokepoint.
#[allow(clippy::too_many_arguments)]
pub fn try_buyback(
    state: &mut SigilState,
    height: u64,
    now_ts: u64,
    caller: WalletId,
    trigger_fee_paid: u128,
    pool_id: sigil_state::PoolId,
    twap_sigil_per_sshare: u128,
) -> Result<BuybackResult, BuybackError> {
    // Gate 1: trigger fee.
    if trigger_fee_paid < MINT_TRIGGER_FEE {
        return Err(BuybackError::TriggerFeeInsufficient {
            paid: trigger_fee_paid,
            required: MINT_TRIGGER_FEE,
        });
    }
    // Gate 2: cooldown.
    let eligible_at = next_buyback_eligible_at_height(state);
    if dec_u64(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_LAST_BUYBACK_HEIGHT)) != 0
        && height < eligible_at
    {
        return Err(BuybackError::CooldownActive { current_height: height, eligible_at });
    }
    let pool_state = state.pool(&pool_id).cloned().ok_or(BuybackError::PoolPairMismatch)?;
    let pool = DexPoolSnapshot::from_pool(&pool_state, twap_sigil_per_sshare)
        .ok_or(BuybackError::PoolPairMismatch)?;
    // Gate 3: THE thin-pool guard (kept).
    if !pool.is_sufficiently_deep() {
        return Err(BuybackError::PoolTooShallow {
            current_sigil_reserves: pool.sigil_reserves,
            required: MIN_POOL_DEPTH_SIGIL,
        });
    }
    // Gate 4: discount threshold — on BOTH TWAP and live spot (see the
    // matching mint gate: the buyback EXECUTES at spot, so paying a
    // spot price above NAV while TWAP claims a discount would burn NAV.
    // Found by the LANE-W chronos gate; QSHARE gated on TWAP alone).
    let premium = premium_ratio_bps(state, &pool, now_ts).ok_or(BuybackError::NavOracleStale)?;
    if premium > DISCOUNT_THRESHOLD_BPS {
        return Err(BuybackError::DiscountAboveThreshold {
            current_bps: premium,
            required_bps: DISCOUNT_THRESHOLD_BPS,
        });
    }
    let spot = DexPoolSnapshot::spot_price(pool.sigil_reserves, pool.sshare_reserves);
    let spot_pool = DexPoolSnapshot { twap_sigil_per_sshare: spot, ..pool };
    let spot_premium =
        premium_ratio_bps(state, &spot_pool, now_ts).ok_or(BuybackError::NavOracleStale)?;
    if spot_premium > DISCOUNT_THRESHOLD_BPS {
        return Err(BuybackError::DiscountAboveThreshold {
            current_bps: spot_premium,
            required_bps: DISCOUNT_THRESHOLD_BPS,
        });
    }

    // Gate 5: yield availability — accrued yield ONLY, never principal.
    let positions = treasury_positions(state);
    let accrued_yield: u128 = positions
        .iter()
        .map(|p| p.value_at(now_ts).saturating_sub(p.principal))
        .fold(0u128, |a, b| a.saturating_add(b));
    let pool_cap = pool
        .sigil_reserves
        .saturating_mul(MAX_BUYBACK_POOL_FRACTION_BPS as u128)
        / 10_000;
    let spend_total = accrued_yield.min(pool_cap);
    if spend_total == 0 {
        return Err(BuybackError::InsufficientYield { available: accrued_yield, needed: 1 });
    }

    // Bounty comes OUT of the spend (conservation-exact): pool receives
    // spend_total - bounty; the caller receives the bounty.
    let bounty =
        (spend_total.saturating_mul(MINT_BOUNTY_BPS as u128) / 10_000).min(MAX_MINT_BOUNTY / 2);
    let sigil_to_pool = spend_total.saturating_sub(bounty);
    if sigil_to_pool == 0 {
        return Err(BuybackError::InsufficientYield { available: accrued_yield, needed: 1 });
    }

    // Swap simulation SIGIL → SSHARE; the bought SSHARE is burned.
    let new_sigil_reserves = pool
        .sigil_reserves
        .checked_add(sigil_to_pool)
        .ok_or(BuybackError::ComputationOverflow)?;
    let sshare_out = pool
        .sshare_reserves
        .saturating_mul(sigil_to_pool)
        / new_sigil_reserves.max(1);
    if sshare_out == 0 {
        return Err(BuybackError::InsufficientYield { available: accrued_yield, needed: 1 });
    }

    let circ = circulating_sshare(state);
    let new_circ = circ.saturating_sub(sshare_out);
    let pool_after = shift_pool(&pool_state, 0, 0, sigil_to_pool, sshare_out)
        .ok_or(BuybackError::ComputationOverflow)?;

    // Realize the yield: every position's accrual baseline resets to now_ts;
    // the realized total R is credited to the treasury wallet, which then
    // pays spend_total. Net treasury delta = R - spend_total ≥ 0, so
    // principal is untouched by construction.
    let realized: u128 = accrued_yield;
    let treasury_bal = state.balance_of(&SSHARE_TREASURY, &NATIVE);
    let treasury_after = treasury_bal
        .saturating_add(realized)
        .saturating_sub(spend_total);

    let new_total = {
        // Post-harvest NAV: positions back at principal, treasury holds the
        // un-spent remainder.
        let principal_total: u128 = positions
            .iter()
            .map(|p| p.principal)
            .fold(0u128, |a, b| a.saturating_add(b));
        treasury_after.saturating_add(principal_total)
    };
    let new_nav = if new_circ == 0 {
        0
    } else {
        new_total.checked_mul(ONE_SSHARE).unwrap_or(u128::MAX) / new_circ
    };

    let event = ShareBuybackEvent {
        burned_sshare: sshare_out,
        sigil_spent: sigil_to_pool,
        new_nav_per_sshare: new_nav,
        discount_ratio_bps: premium,
        trigger_caller: caller,
        bounty_paid_sigil: bounty,
        block_height: height,
    };

    let ops = state.contract_slot(&SSHARE_CONTRACT, &SLOT_LIFETIME_OPS);
    let mints = u64::from_le_bytes(ops[0..8].try_into().unwrap());
    let buybacks = u64::from_le_bytes(ops[8..16].try_into().unwrap()).saturating_add(1);
    let mut ops_enc = [0u8; 32];
    ops_enc[0..8].copy_from_slice(&mints.to_le_bytes());
    ops_enc[8..16].copy_from_slice(&buybacks.to_le_bytes());
    let burned_total = dec_u128(&state.contract_slot(&SSHARE_CONTRACT, &SLOT_LIFETIME_BURNED))
        .saturating_add(sshare_out);

    let mut mutations: Vec<StateMutation> = Vec::with_capacity(12 + positions.len());
    // Harvest: reset every position's accrual baseline.
    for (i, pos) in positions.iter().enumerate() {
        let harvested = TreasuryPosition { locked_at_timestamp: now_ts, ..*pos };
        mutations.push(StateMutation::SetContractSlot {
            contract: SSHARE_CONTRACT,
            slot: position_slot(i as u64),
            value: harvested.encode(),
        });
    }
    if caller == SSHARE_TREASURY {
        mutations.push(StateMutation::SetBalance {
            wallet: SSHARE_TREASURY,
            token: NATIVE,
            amount: treasury_after.saturating_add(bounty),
        });
    } else {
        let cbal = state.balance_of(&caller, &NATIVE);
        mutations.push(StateMutation::SetBalance {
            wallet: SSHARE_TREASURY,
            token: NATIVE,
            amount: treasury_after,
        });
        mutations.push(StateMutation::SetBalance {
            wallet: caller,
            token: NATIVE,
            amount: cbal.saturating_add(bounty),
        });
    }
    mutations.push(StateMutation::SetPool { pool: pool_id, state: pool_after });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_SUPPLY,
        value: enc_u128(new_circ),
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_LAST_BUYBACK_HEIGHT,
        value: enc_u64(height),
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_NAV_ORACLE,
        value: NavOracleState {
            last_nav_per_sshare: new_nav,
            last_computed_height: height,
            last_computed_timestamp: now_ts,
        }
        .encode(),
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_LIFETIME_OPS,
        value: ops_enc,
    });
    mutations.push(StateMutation::SetContractSlot {
        contract: SSHARE_CONTRACT,
        slot: SLOT_LIFETIME_BURNED,
        value: enc_u128(burned_total),
    });
    mutations.push(StateMutation::PushEventHash(event.hash()));

    let t = StateTransition { at_height: height, mutations };
    commit_state_transition(state, &t, height)?;

    Ok(BuybackResult {
        event,
        bounty_paid_to_caller: bounty,
        new_circulating_sshare: new_circ,
    })
}

// ============ ADMIN / LIFECYCLE ============

/// One-shot bootstrap: seed the initial SSHARE supply + treasury backing.
///
/// `funder` pays `treasury_sigil` of NATIVE into the [`SSHARE_TREASURY`]
/// bank-pool wallet (conserved — moved, not minted) and receives
/// `initial_sshare` of freshly created SSHARE. Only legal while the supply
/// slot is zero; afterwards supply moves ONLY via mint/buyback. Initial
/// NAV = treasury_sigil / initial_sshare.
pub fn bootstrap(
    state: &mut SigilState,
    height: u64,
    funder: WalletId,
    treasury_sigil: u128,
    initial_sshare: u128,
) -> Result<(), ShareAdminError> {
    if circulating_sshare(state) != 0 {
        return Err(ShareAdminError::AlreadyBootstrapped);
    }
    if treasury_sigil == 0 || initial_sshare == 0 || funder == SSHARE_TREASURY {
        // funder == treasury would double-write the same wallet in one
        // transition with a stale pre-read — refuse the degenerate call.
        return Err(ShareAdminError::ZeroAmount);
    }
    let funder_native = state.balance_of(&funder, &NATIVE);
    if funder_native < treasury_sigil {
        return Err(ShareAdminError::InsufficientBalance { have: funder_native, need: treasury_sigil });
    }
    let treasury_bal = state.balance_of(&SSHARE_TREASURY, &NATIVE);
    let funder_sshare = state.balance_of(&funder, &SSHARE);
    let t = StateTransition {
        at_height: height,
        mutations: vec![
            StateMutation::SetBalance {
                wallet: funder,
                token: NATIVE,
                amount: funder_native - treasury_sigil,
            },
            StateMutation::SetBalance {
                wallet: SSHARE_TREASURY,
                token: NATIVE,
                amount: treasury_bal.saturating_add(treasury_sigil),
            },
            StateMutation::SetBalance {
                wallet: funder,
                token: SSHARE,
                amount: funder_sshare.saturating_add(initial_sshare),
            },
            StateMutation::SetContractSlot {
                contract: SSHARE_CONTRACT,
                slot: SLOT_SUPPLY,
                value: enc_u128(initial_sshare),
            },
        ],
    };
    commit_state_transition(state, &t, height)?;
    Ok(())
}

/// Lock `principal` out of the treasury pending pool into a yield-bearing
/// position (the QSHARE `record_qcredit_position` port — LANE-X wires real
/// QCREDIT tiers into `yield_bps`). Principal parks in
/// [`SSHARE_POSITION_VAULT`] (conserved); the position record is committed
/// as a contract slot.
pub fn record_treasury_position(
    state: &mut SigilState,
    height: u64,
    yield_bps: u32,
    principal: u128,
    now_ts: u64,
) -> Result<(), ShareAdminError> {
    if principal == 0 {
        return Err(ShareAdminError::ZeroAmount);
    }
    let pending = state.balance_of(&SSHARE_TREASURY, &NATIVE);
    if pending < principal {
        return Err(ShareAdminError::InsufficientBalance { have: pending, need: principal });
    }
    let vault = state.balance_of(&SSHARE_POSITION_VAULT, &NATIVE);
    let count = position_count(state);
    let pos = TreasuryPosition { principal, yield_bps, locked_at_timestamp: now_ts };
    let t = StateTransition {
        at_height: height,
        mutations: vec![
            StateMutation::SetBalance {
                wallet: SSHARE_TREASURY,
                token: NATIVE,
                amount: pending - principal,
            },
            StateMutation::SetBalance {
                wallet: SSHARE_POSITION_VAULT,
                token: NATIVE,
                amount: vault.saturating_add(principal),
            },
            StateMutation::SetContractSlot {
                contract: SSHARE_CONTRACT,
                slot: position_slot(count),
                value: pos.encode(),
            },
            StateMutation::SetContractSlot {
                contract: SSHARE_CONTRACT,
                slot: SLOT_POSITION_COUNT,
                value: enc_u64(count + 1),
            },
        ],
    };
    commit_state_transition(state, &t, height)?;
    Ok(())
}

// NOTE on NAV accounting for positions: a position's principal physically
// parks in SSHARE_POSITION_VAULT but is counted via the position record in
// `nav_total_sigil_equivalent` (principal + accrued yield); the vault wallet
// itself is deliberately NOT summed — that would double-count.

// ============ TESTS ============

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::PoolId;

    const FUNDER: WalletId = [0x11; 32];
    const CALLER: WalletId = [0x22; 32];
    const POOL: PoolId = [0x77; 32];

    /// Fund `wallet` with `amount` NATIVE via the chokepoint.
    fn fund(state: &mut SigilState, height: u64, wallet: WalletId, amount: u128) {
        let bal = state.balance_of(&wallet, &NATIVE);
        let t = StateTransition {
            at_height: height,
            mutations: vec![StateMutation::SetBalance {
                wallet,
                token: NATIVE,
                amount: bal + amount,
            }],
        };
        commit_state_transition(state, &t, height).unwrap();
    }

    /// Install a SIGIL/SSHARE pool via the chokepoint.
    fn install_pool(state: &mut SigilState, height: u64, sigil: u128, sshare: u128) {
        let t = StateTransition {
            at_height: height,
            mutations: vec![StateMutation::SetPool {
                pool: POOL,
                state: PoolState {
                    token_a: NATIVE,
                    token_b: SSHARE,
                    reserve_a: sigil,
                    reserve_b: sshare,
                    lp_shares: 1,
                    fee_bps: 30,
                    accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(state, &t, height).unwrap();
    }

    /// Bootstrapped state: 1000 SSHARE circulating, 1000 SIGIL treasury →
    /// NAV = 1 SIGIL/SSHARE.
    fn genesis() -> SigilState {
        let mut s = SigilState::new();
        fund(&mut s, 0, FUNDER, 2_000 * ONE_SSHARE);
        fund(&mut s, 0, CALLER, ONE_SSHARE);
        bootstrap(&mut s, 1, FUNDER, 1_000 * ONE_SSHARE, 1_000 * ONE_SSHARE).unwrap();
        s
    }

    #[test]
    fn fresh_state_has_zero_nav() {
        let s = SigilState::new();
        assert_eq!(nav_per_sshare(&s, 0), 0);
        assert_eq!(circulating_sshare(&s), 0);
    }

    #[test]
    fn bootstrap_sets_supply_and_nav() {
        let s = genesis();
        assert_eq!(circulating_sshare(&s), 1_000 * ONE_SSHARE);
        assert_eq!(s.balance_of(&SSHARE_TREASURY, &NATIVE), 1_000 * ONE_SSHARE);
        assert_eq!(s.balance_of(&FUNDER, &SSHARE), 1_000 * ONE_SSHARE);
        // NAV = 1000 SIGIL / 1000 SSHARE = 1.0 (×ONE_SSHARE fixed point)
        assert_eq!(nav_per_sshare(&s, 0), ONE_SSHARE);
        // one-shot: second bootstrap refused
        let mut s2 = s;
        assert!(matches!(
            bootstrap(&mut s2, 2, FUNDER, ONE_SSHARE, ONE_SSHARE),
            Err(ShareAdminError::AlreadyBootstrapped)
        ));
    }

    #[test]
    fn mint_below_threshold_rejected() {
        let mut s = genesis();
        // Market = 1.0 = NAV → premium 1000 < 1500.
        install_pool(&mut s, 2, 10_000 * ONE_SSHARE, 10_000 * ONE_SSHARE);
        let r = try_autonomous_mint(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, ONE_SSHARE);
        match r {
            Err(MintError::PremiumBelowThreshold { current_bps, required_bps }) => {
                assert!(current_bps < required_bps);
            }
            other => panic!("expected PremiumBelowThreshold, got {other:?}"),
        }
    }

    #[test]
    fn mint_above_threshold_succeeds_and_commits() {
        let mut s = genesis();
        // Market = 2.0 → premium 2000 ≥ 1500.
        install_pool(&mut s, 2, 10_000 * ONE_SSHARE, 5_000 * ONE_SSHARE);
        let treasury_before = s.balance_of(&SSHARE_TREASURY, &NATIVE);
        let caller_before = s.balance_of(&CALLER, &NATIVE);
        let circ_before = circulating_sshare(&s);

        let r = try_autonomous_mint(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, 2 * ONE_SSHARE)
            .expect("mint should succeed at 2× premium");
        assert!(r.event.minted_sshare > 0);
        assert!(r.event.sigil_accumulated > 0);
        assert!(r.bounty_paid_to_caller > 0);

        // Committed effects — all through the chokepoint, all in the roots:
        assert_eq!(circulating_sshare(&s), circ_before + r.event.minted_sshare, "supply slot grew");
        assert_eq!(
            s.balance_of(&SSHARE_TREASURY, &NATIVE),
            treasury_before + r.event.sigil_accumulated,
            "treasury bank-pool credited"
        );
        assert_eq!(
            s.balance_of(&CALLER, &NATIVE),
            caller_before + r.bounty_paid_to_caller,
            "bounty paid"
        );
        // Pool shifted: SSHARE in, SIGIL out.
        let p = s.pool(&POOL).unwrap();
        assert_eq!(p.reserve_b, 5_000 * ONE_SSHARE + r.event.minted_sshare);
        assert_eq!(
            p.reserve_a,
            10_000 * ONE_SSHARE - r.event.sigil_accumulated - r.bounty_paid_to_caller
        );
        // NAV oracle refreshed.
        assert_eq!(nav_oracle(&s).last_nav_per_sshare, r.event.new_nav_per_sshare);
        assert_eq!(nav_oracle(&s).last_computed_height, 1000);
    }

    #[test]
    fn mint_is_nav_accretive() {
        let mut s = genesis();
        install_pool(&mut s, 2, 10_000 * ONE_SSHARE, 5_000 * ONE_SSHARE);
        let nav_before = nav_per_sshare(&s, 0);
        try_autonomous_mint(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, 2 * ONE_SSHARE).unwrap();
        let nav_after = nav_per_sshare(&s, 0);
        assert!(
            nav_after >= nav_before,
            "selling shares above NAV must raise NAV: {nav_before} -> {nav_after}"
        );
    }

    #[test]
    fn mint_cooldown_enforced() {
        let mut s = genesis();
        install_pool(&mut s, 2, 10_000 * ONE_SSHARE, 5_000 * ONE_SSHARE);
        try_autonomous_mint(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, 2 * ONE_SSHARE).unwrap();
        // 1000 + 360 = 1360. height=1100 must fail.
        let r = try_autonomous_mint(&mut s, 1100, 0, CALLER, MINT_TRIGGER_FEE, POOL, 2 * ONE_SSHARE);
        match r {
            Err(MintError::CooldownActive { current_height, eligible_at }) => {
                assert_eq!(current_height, 1100);
                assert_eq!(eligible_at, 1360);
            }
            other => panic!("expected CooldownActive, got {other:?}"),
        }
    }

    #[test]
    fn mint_pool_fraction_cap_enforced() {
        let mut s = SigilState::new();
        fund(&mut s, 0, FUNDER, 2_000 * ONE_SSHARE);
        fund(&mut s, 0, CALLER, ONE_SSHARE);
        // 1M circulating, tiny treasury → NAV ≈ 0.001 → huge premium at market 1.0.
        bootstrap(&mut s, 1, FUNDER, 1_000 * ONE_SSHARE, 1_000_000 * ONE_SSHARE).unwrap();
        install_pool(&mut s, 2, 10_000 * ONE_SSHARE, 10_000 * ONE_SSHARE);
        let r = try_autonomous_mint(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, ONE_SSHARE)
            .expect("huge premium should mint");
        // Cap1 = 0.5% of 10000 SSHARE = 50; Cap2 = 2% of 1M = 20_000 → active cap 50.
        assert!(r.event.minted_sshare <= 50 * ONE_SSHARE + 1);
    }

    #[test]
    fn thin_pool_mint_refused() {
        // THE guard the LANE-W spec pins: shallow pool → no mint, ever.
        let mut s = genesis();
        // 999 SIGIL < MIN_POOL_DEPTH_SIGIL (1000 SIGIL).
        install_pool(&mut s, 2, 999 * ONE_SSHARE, 500 * ONE_SSHARE);
        let r = try_autonomous_mint(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, 2 * ONE_SSHARE);
        match r {
            Err(MintError::PoolTooShallow { current_sigil_reserves, required }) => {
                assert_eq!(current_sigil_reserves, 999 * ONE_SSHARE);
                assert_eq!(required, MIN_POOL_DEPTH_SIGIL);
            }
            other => panic!("expected PoolTooShallow, got {other:?}"),
        }
    }

    #[test]
    fn thin_pool_buyback_refused() {
        let mut s = genesis();
        install_pool(&mut s, 2, 999 * ONE_SSHARE, 2_000 * ONE_SSHARE);
        let r = try_buyback(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, ONE_SSHARE / 2);
        assert!(matches!(r, Err(BuybackError::PoolTooShallow { .. })));
    }

    #[test]
    fn treasury_position_yield_accrues() {
        let pos = TreasuryPosition {
            principal: 1_000 * ONE_SSHARE,
            yield_bps: 2_500, // 25% APY — QSHARE's Platinum tier
            locked_at_timestamp: 0,
        };
        let value_after_1y = pos.value_at(SECONDS_PER_YEAR);
        let expected = 1_250 * ONE_SSHARE;
        let diff = value_after_1y.abs_diff(expected);
        assert!(diff < ONE_SSHARE / 100, "expected ~1250 SIGIL, got {value_after_1y} (diff {diff})");
    }

    #[test]
    fn record_position_moves_principal_to_vault() {
        let mut s = genesis();
        record_treasury_position(&mut s, 2, 2_500, 500 * ONE_SSHARE, 1_000).unwrap();
        assert_eq!(s.balance_of(&SSHARE_TREASURY, &NATIVE), 500 * ONE_SSHARE, "pending shrank");
        assert_eq!(s.balance_of(&SSHARE_POSITION_VAULT, &NATIVE), 500 * ONE_SSHARE, "vault holds principal");
        let positions = treasury_positions(&s);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].principal, 500 * ONE_SSHARE);
        // NAV unchanged at lock time: pending(500) + position(500) = 1000.
        assert_eq!(nav_per_sshare(&s, 1_000), ONE_SSHARE);
    }

    #[test]
    fn buyback_without_yield_refused() {
        let mut s = genesis();
        // Discounted market (0.5× NAV) but no positions → zero yield.
        install_pool(&mut s, 2, 10_000 * ONE_SSHARE, 20_000 * ONE_SSHARE);
        let r = try_buyback(&mut s, 1000, 0, CALLER, MINT_TRIGGER_FEE, POOL, ONE_SSHARE / 2);
        assert!(matches!(r, Err(BuybackError::InsufficientYield { .. })));
    }

    #[test]
    fn buyback_at_discount_burns_and_never_spends_principal() {
        let mut s = genesis();
        // Lock 500 into a 25% position, accrue 1 year of yield (= 125 SIGIL).
        record_treasury_position(&mut s, 2, 2_500, 500 * ONE_SSHARE, 0).unwrap();
        // Market = 0.5× NAV-ish → discounted.
        install_pool(&mut s, 3, 10_000 * ONE_SSHARE, 20_000 * ONE_SSHARE);
        let now = SECONDS_PER_YEAR;
        let circ_before = circulating_sshare(&s);
        let pending_before = s.balance_of(&SSHARE_TREASURY, &NATIVE);

        let r = try_buyback(&mut s, 1000, now, CALLER, MINT_TRIGGER_FEE, POOL, ONE_SSHARE / 2)
            .expect("discounted + yielding treasury should buy back");
        assert!(r.event.burned_sshare > 0);
        // Supply slot decreased by the burn.
        assert_eq!(circulating_sshare(&s), circ_before - r.event.burned_sshare);
        // Principal never spent: pending can only have grown (yield remainder
        // lands there) and the position principal is intact.
        assert!(s.balance_of(&SSHARE_TREASURY, &NATIVE) >= pending_before);
        assert_eq!(treasury_positions(&s)[0].principal, 500 * ONE_SSHARE);
        // Accrual baseline reset on harvest.
        assert_eq!(treasury_positions(&s)[0].locked_at_timestamp, now);
        // Pool shifted: SIGIL in, SSHARE out.
        let p = s.pool(&POOL).unwrap();
        assert_eq!(p.reserve_a, 10_000 * ONE_SSHARE + r.event.sigil_spent);
        assert_eq!(p.reserve_b, 20_000 * ONE_SSHARE - r.event.burned_sshare);
    }

    #[test]
    fn buyback_is_nav_accretive() {
        let mut s = genesis();
        record_treasury_position(&mut s, 2, 2_500, 500 * ONE_SSHARE, 0).unwrap();
        install_pool(&mut s, 3, 10_000 * ONE_SSHARE, 20_000 * ONE_SSHARE);
        let now = SECONDS_PER_YEAR;
        let nav_before = nav_per_sshare(&s, now);
        try_buyback(&mut s, 1000, now, CALLER, MINT_TRIGGER_FEE, POOL, ONE_SSHARE / 2).unwrap();
        let nav_after = nav_per_sshare(&s, now);
        assert!(
            nav_after >= nav_before,
            "burning shares below NAV must raise NAV: {nav_before} -> {nav_after}"
        );
    }

    /// Port of token_contract_test.rs's conservation property: transfers of
    /// SSHARE between wallets (via the chokepoint) never change the supply
    /// slot, and the sum of wallet balances stays equal to the non-pool
    /// supply.
    #[test]
    fn transfers_conserve_supply() {
        let mut s = genesis();
        let supply = circulating_sshare(&s);
        let alice_before = s.balance_of(&FUNDER, &SSHARE);
        // FUNDER → CALLER: 300 SSHARE.
        let amt = 300 * ONE_SSHARE;
        let t = StateTransition {
            at_height: 2,
            mutations: vec![
                StateMutation::SetBalance { wallet: FUNDER, token: SSHARE, amount: alice_before - amt },
                StateMutation::SetBalance { wallet: CALLER, token: SSHARE, amount: amt },
            ],
        };
        commit_state_transition(&mut s, &t, 2).unwrap();
        assert_eq!(circulating_sshare(&s), supply, "supply slot untouched by transfers");
        assert_eq!(
            s.balance_of(&FUNDER, &SSHARE) + s.balance_of(&CALLER, &SSHARE),
            supply,
            "wallet balances conserve the committed supply"
        );
    }

    /// LANE-W acceptance: a mint+buyback roundtrip must not move NAV by more
    /// than the fee/bounty slice — and in this design both legs are
    /// accretive, so NAV must end ≥ where it started.
    #[test]
    fn nav_invariant_mint_buyback_roundtrip() {
        let mut s = genesis();
        record_treasury_position(&mut s, 2, 2_500, 500 * ONE_SSHARE, 0).unwrap();
        install_pool(&mut s, 3, 10_000 * ONE_SSHARE, 5_000 * ONE_SSHARE);

        let nav_start = nav_per_sshare(&s, 0);
        // Leg 1: mint at 2× premium.
        try_autonomous_mint(&mut s, 1_000, 0, CALLER, MINT_TRIGGER_FEE, POOL, 2 * ONE_SSHARE).unwrap();
        let nav_mid = nav_per_sshare(&s, 0);
        assert!(nav_mid >= nav_start, "mint leg must not dilute NAV");

        // Leg 2: a year later the market has crashed to a discount; the
        // pool itself must trade there too (the spot cross-check refuses a
        // TWAP-only "discount" — that's the manipulation guard working).
        let now = SECONDS_PER_YEAR;
        let nav_pre_bb = nav_per_sshare(&s, now);
        set_pool_spot(&mut s, 1_500, nav_pre_bb / 2);
        try_buyback(&mut s, 2_000, now, CALLER, MINT_TRIGGER_FEE, POOL, nav_pre_bb / 2).unwrap();
        let nav_end = nav_per_sshare(&s, now);

        // Tolerance = the bounty slice (MINT_BOUNTY_BPS of the leg volumes) —
        // the only value that leaves the treasury/pool system.
        let tol = nav_pre_bb.saturating_mul(MINT_BOUNTY_BPS as u128) / 10_000 + 1;
        assert!(
            nav_end + tol >= nav_pre_bb,
            "roundtrip NAV drift exceeds the fee slice: {nav_pre_bb} -> {nav_end} (tol {tol})"
        );
        assert!(nav_end >= nav_start, "full roundtrip must be NAV-accretive");
    }

    /// Re-price the pool so its SPOT equals `price_fp` (SIGIL per SSHARE,
    /// ×ONE_SSHARE) — the market moving between regimes. Through the
    /// chokepoint like everything else.
    fn set_pool_spot(state: &mut SigilState, height: u64, price_fp: u128) {
        let sigil = 50_000 * ONE_SSHARE;
        let sshare = sigil.saturating_mul(ONE_SSHARE) / price_fp.max(1);
        let prev = state.pool(&POOL).cloned().expect("pool installed");
        let t = StateTransition {
            at_height: height,
            mutations: vec![StateMutation::SetPool {
                pool: POOL,
                state: PoolState { reserve_a: sigil, reserve_b: sshare, ..prev },
            }],
        };
        commit_state_transition(state, &t, height).unwrap();
    }

    /// CHRONOS GATE (LANE-W): drive years of virtual time through repeated
    /// mint/buyback cycles at two very different block rates and assert
    /// (1) NAV per share NEVER decreases beyond the fee slice on any single
    /// op, (2) NAV ends above where it started (reflexive accretion), and
    /// (3) the committed supply slot tracks every mint/burn exactly.
    /// (This gate caught the TWAP/spot divergence — the spot cross-check in
    /// both gates exists because of it.)
    #[test]
    fn chronos_nav_drift_gate() {
        for blocks_per_cycle in [1_000u64, 100_000u64] {
            // fast chain vs slow chain
            let mut s = genesis();
            record_treasury_position(&mut s, 2, 2_500, 500 * ONE_SSHARE, 0).unwrap();
            install_pool(&mut s, 3, 50_000 * ONE_SSHARE, 25_000 * ONE_SSHARE);

            let nav_start = nav_per_sshare(&s, 0);
            let mut height = 10_000u64;
            let mut now = 0u64;
            let mut expected_supply = circulating_sshare(&s);

            for cycle in 0..50u32 {
                height += blocks_per_cycle;
                now += 90 * 24 * 3600; // a quarter of virtual time per cycle
                let nav = nav_per_sshare(&s, now);
                let tol = nav.saturating_mul(MINT_BOUNTY_BPS as u128) / 10_000 + 1;

                if cycle % 2 == 0 {
                    // premium regime: market trades at 2× NAV
                    let twap = nav.saturating_mul(2);
                    set_pool_spot(&mut s, height, twap);
                    match try_autonomous_mint(&mut s, height, now, CALLER, MINT_TRIGGER_FEE, POOL, twap) {
                        Ok(r) => {
                            expected_supply += r.event.minted_sshare;
                            let nav_after = nav_per_sshare(&s, now);
                            assert!(
                                nav_after + tol >= nav,
                                "cycle {cycle}: mint dropped NAV beyond fee ({nav} -> {nav_after})"
                            );
                        }
                        Err(MintError::CooldownActive { .. }) => {}
                        Err(e) => panic!("cycle {cycle}: unexpected mint error {e:?}"),
                    }
                } else {
                    // discount regime: market trades at 0.5× NAV
                    let twap = nav / 2;
                    set_pool_spot(&mut s, height, twap);
                    match try_buyback(&mut s, height, now, CALLER, MINT_TRIGGER_FEE, POOL, twap) {
                        Ok(r) => {
                            expected_supply -= r.event.burned_sshare;
                            let nav_after = nav_per_sshare(&s, now);
                            assert!(
                                nav_after + tol >= nav,
                                "cycle {cycle}: buyback dropped NAV beyond fee ({nav} -> {nav_after})"
                            );
                        }
                        Err(BuybackError::CooldownActive { .. })
                        | Err(BuybackError::InsufficientYield { .. }) => {}
                        Err(e) => panic!("cycle {cycle}: unexpected buyback error {e:?}"),
                    }
                }
                assert_eq!(
                    circulating_sshare(&s),
                    expected_supply,
                    "cycle {cycle}: committed supply slot diverged from the op ledger"
                );
            }

            let nav_end = nav_per_sshare(&s, now);
            assert!(
                nav_end >= nav_start,
                "chronos({blocks_per_cycle} blk/cycle): NAV drifted DOWN over the run: {nav_start} -> {nav_end}"
            );
        }
    }

    #[test]
    fn registry_metadata_registers_sshare() {
        use sigil_dex::registry::TokenRegistry;
        let s = genesis();
        let mut reg = TokenRegistry::new();
        let meta = sshare_metadata(1, circulating_sshare(&s));
        reg.register_token(meta).unwrap();
        let row = reg.token_by_symbol(SSHARE_SYMBOL).expect("SSHARE registered");
        assert_eq!(row.token_id, SSHARE);
        assert_eq!(row.decimals, 8, "SIGIL convention — NOT QUG's 24");
        assert_eq!(row.total_supply, 1_000 * ONE_SSHARE);
    }

    #[test]
    fn event_hashes_are_domain_separated() {
        let m = ShareMintEvent {
            minted_sshare: 1,
            sigil_accumulated: 1,
            new_nav_per_sshare: 1,
            premium_ratio_bps: 1,
            trigger_caller: [0; 32],
            bounty_paid_sigil: 1,
            block_height: 1,
        };
        let b = ShareBuybackEvent {
            burned_sshare: 1,
            sigil_spent: 1,
            new_nav_per_sshare: 1,
            discount_ratio_bps: 1,
            trigger_caller: [0; 32],
            bounty_paid_sigil: 1,
            block_height: 1,
        };
        assert_ne!(m.hash(), b.hash(), "mint/buyback events must never collide");
    }
}
