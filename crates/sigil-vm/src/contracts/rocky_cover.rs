//! ROCKY — Rocky's token, and the first contract that settles on a Kristensen gauge.
//!
//! Viktor, 2026-09-15: "lav den første native kontrakt — K⊕-p99 insurance og det skal være
//! rockys token. også includer advanced smart contract features (dem vi også har i quillon) så
//! feks reflection, staking og airdrop osv er muligt".
//!
//! One native contract, one token, six features, all state in contract slots and the wallet
//! SMT — every write goes through `commit_state_transition` in the same transition as the
//! tx that caused it (the Quillon-postmortem rule; see `sigil_share.rs`).
//!
//! | feature (Quillon AdvancedToken template) | here |
//! |---|---|
//! | mintable / burnable / pausable | owner mints and pauses; anyone burns their own |
//! | reflection | a fee (bps) on every transfer is redistributed to holders pro-rata, by a lazily-settled index — no holder iteration, exact to dust |
//! | staking | stake ROCKY into the cover pool for SHARES (NAV = pool / shares); premiums raise the NAV, claims lower it — stakers are the underwriters |
//! | airdrop | owner credits up to 256 wallets in one call |
//! | governance | NOT here (say so) — ownership is one wallet, transferable |
//! | **cover** (new) | buy K⊕ cover for a date window; claim pays from the pool when the on-chain `earth.k_resid` reading exceeds the on-chain `earth.k_p99` reading inside the window |
//!
//! ## Who calls it
//! `SigilTx::ContractCall { contract: ROCKY_CONTRACT, method: selector(name), calldata: JSON(RockyCall) }`,
//! dispatched by `sigil-tx` behind the same activation height as `GaugePush`
//! (`sigil_oracle::gauge_active`). The caller is the tx's `from` — authenticated at the door,
//! consensus-checked here against the owner/master slots. Every call emits exactly ONE typed
//! `SigilEvent::ContractCall { result_hash }` (the header↔body gate requires typed events;
//! `result_hash` is the domain-separated digest of what happened).
//!
//! ## What is honest about the insurance
//! The trigger is two consensus-committed numbers (`GaugePush`), not an oracle promise: K⊕ on a
//! date inside the policy window, above that day's p99. A claim pays `min(cover, pool)` — the
//! pool can be thinner than the cover if many claim at once; that is what underwriting means and
//! the number is visible before you buy. Premiums are set by the buyer (a market, not a formula):
//! the contract only enforces `premium ≥ cover × MIN_PREMIUM_BPS / 10 000`.

use serde::{Deserialize, Serialize};
use sigil_oracle::{feed_id, gauge_is_fresh, read_gauge};
use sigil_state::{u128_str, ContractId, SigilState, SlotId, StateMutation, TokenId, WalletId};

// ============ IDENTITY ============
/// ROCKY token id in the wallet SMT (distinct from NATIVE 00, SSHARE 55, QCREDIT C1, USDS D5).
pub const ROCKY: TokenId = [0x4B; 32];
/// Symbol and name.
pub const ROCKY_SYMBOL: &str = "ROCKY";
pub const ROCKY_NAME: &str = "Rocky K\u{2295} Cover";
/// The contract id under which every ROCKY slot lives.
pub const ROCKY_CONTRACT: ContractId = [0x5D; 32];
/// Decimals — SIGIL convention (10).
pub const ROCKY_DECIMALS: u32 = sigil_state::SIGIL_DECIMALS;
pub const ONE_ROCKY: u128 = 10u128.pow(ROCKY_DECIMALS);
const _: () = assert!(ONE_ROCKY == 10u128.pow(sigil_state::SIGIL_DECIMALS));

// ============ RULES ============
/// Reflection fee ceiling: 10 %.
pub const MAX_FEE_BPS: u64 = 1_000;
/// Default reflection fee: 2 %.
pub const DEFAULT_FEE_BPS: u64 = 200;
/// Magnification of the reflection index (fee × MAG / circulating).
pub const MAG: u128 = 1_000_000_000_000_000_000; // 1e18
/// Airdrop: at most this many recipients per call.
pub const MAX_AIRDROP: usize = 256;
/// Blocks a staker waits between `Stake` and `Unstake` (no flash-underwriting a known claim).
pub const UNSTAKE_COOLDOWN_BLOCKS: u64 = 4_096;
/// A cover cannot exceed the pool at purchase time.
pub const MAX_COVER_OF_POOL_BPS: u64 = 10_000;
/// Minimum premium: 1 % of the cover.
pub const MIN_PREMIUM_BPS: u64 = 100;
/// Maximum policies ever (slot space is cheap; a bound is honest).
pub const MAX_POLICIES: u64 = 1 << 32;
/// The two feeds the cover settles on.
pub const FEED_K: &str = "earth.k_resid";
pub const FEED_P99: &str = "earth.k_p99";

// ============ SLOTS ============
pub const SLOT_OWNER: SlotId = [0x01; 32];
pub const SLOT_PAUSED: SlotId = [0x02; 32];
pub const SLOT_FEE_BPS: SlotId = [0x03; 32];
pub const SLOT_TOTAL_SUPPLY: SlotId = [0x04; 32];
pub const SLOT_REFLECT_INDEX: SlotId = [0x05; 32];
pub const SLOT_REFLECT_POOL: SlotId = [0x06; 32];
pub const SLOT_POOL_BALANCE: SlotId = [0x07; 32];
pub const SLOT_POOL_SHARES: SlotId = [0x08; 32];
pub const SLOT_POLICY_COUNT: SlotId = [0x09; 32];
pub const SLOT_BOOTSTRAPPED: SlotId = [0x0A; 32];

fn wallet_slot(domain: &[u8], w: &WalletId) -> SlotId {
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/rocky/slot/");
    h.update(domain);
    h.update(w);
    *h.finalize().as_bytes()
}
/// The reflection index a wallet was last settled at.
pub fn holder_index_slot(w: &WalletId) -> SlotId {
    wallet_slot(b"holder-index", w)
}
/// A wallet's pool shares.
pub fn shares_slot(w: &WalletId) -> SlotId {
    wallet_slot(b"shares", w)
}
/// The height a wallet last staked at (cooldown).
pub fn staked_at_slot(w: &WalletId) -> SlotId {
    wallet_slot(b"staked-at", w)
}
/// Policy `n` (two slots: terms and holder).
pub fn policy_slot(n: u64) -> SlotId {
    wallet_slot(b"policy", &{ let mut b = [0u8; 32]; b[..8].copy_from_slice(&n.to_le_bytes()); b })
}
pub fn policy_holder_slot(n: u64) -> SlotId {
    wallet_slot(b"policy-holder", &{ let mut b = [0u8; 32]; b[..8].copy_from_slice(&n.to_le_bytes()); b })
}

/// 4-byte method selector = BLAKE3("SIGIL/rocky/v1/" ‖ name)[..4].
pub fn selector(name: &str) -> [u8; 4] {
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/rocky/v1/");
    h.update(name.as_bytes());
    let d = h.finalize();
    [d.as_bytes()[0], d.as_bytes()[1], d.as_bytes()[2], d.as_bytes()[3]]
}

// ============ CALLS ============
/// One airdrop line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drop {
    pub to: WalletId,
    #[serde(with = "u128_str")]
    pub amount: u128,
}

/// The contract's methods. Calldata is the JSON of this enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// flux-wire: allow — JSON only: travels as opaque `calldata: Vec<u8>` inside ContractCall and is decoded by `RockyCall::decode` (serde_json); pinned by calls_round_trip_as_json_and_selectors_are_stable
#[serde(tag = "call", rename_all = "snake_case")]
pub enum RockyCall {
    /// Master-only, once: name the owner (Rocky's wallet) and mint the initial supply to them.
    Bootstrap { owner: WalletId, #[serde(with = "u128_str")] initial_supply: u128 },
    Transfer { to: WalletId, #[serde(with = "u128_str")] amount: u128 },
    Mint { to: WalletId, #[serde(with = "u128_str")] amount: u128 },
    Burn { #[serde(with = "u128_str")] amount: u128 },
    SetFeeBps { bps: u64 },
    Pause { on: bool },
    Airdrop { recipients: Vec<Drop> },
    TransferOwnership { to: WalletId },
    Stake { #[serde(with = "u128_str")] amount: u128 },
    Unstake { #[serde(with = "u128_str")] shares: u128 },
    /// Buy K⊕ cover: `premium` ROCKY into the pool now; `cover` ROCKY paid on a trigger whose
    /// reading date is in `[from_date, until_date]` (YYYYMMDD).
    BuyCover { #[serde(with = "u128_str")] premium: u128, #[serde(with = "u128_str")] cover: u128, from_date: u32, until_date: u32 },
    /// Claim policy `n` — anyone may trigger it; the holder is paid.
    Claim { policy: u64 },
    /// Settle your own reflection owed into your balance (transfers do this implicitly).
    Settle,
}

impl RockyCall {
    pub fn name(&self) -> &'static str {
        match self {
            RockyCall::Bootstrap { .. } => "bootstrap",
            RockyCall::Transfer { .. } => "transfer",
            RockyCall::Mint { .. } => "mint",
            RockyCall::Burn { .. } => "burn",
            RockyCall::SetFeeBps { .. } => "set_fee_bps",
            RockyCall::Pause { .. } => "pause",
            RockyCall::Airdrop { .. } => "airdrop",
            RockyCall::TransferOwnership { .. } => "transfer_ownership",
            RockyCall::Stake { .. } => "stake",
            RockyCall::Unstake { .. } => "unstake",
            RockyCall::BuyCover { .. } => "buy_cover",
            RockyCall::Claim { .. } => "claim",
            RockyCall::Settle => "settle",
        }
    }
    pub fn selector(&self) -> [u8; 4] {
        selector(self.name())
    }
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
    pub fn decode(calldata: &[u8]) -> Result<RockyCall, RockyError> {
        serde_json::from_slice(calldata).map_err(|e| RockyError::BadCalldata(e.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RockyError {
    #[error("calldata is not a RockyCall: {0}")]
    BadCalldata(String),
    #[error("method selector does not match the call")]
    SelectorMismatch,
    #[error("not bootstrapped")]
    NotBootstrapped,
    #[error("already bootstrapped")]
    AlreadyBootstrapped,
    #[error("only the master wallet may bootstrap")]
    NotMaster,
    #[error("only the owner may do that")]
    NotOwner,
    #[error("paused")]
    Paused,
    #[error("zero amount")]
    Zero,
    #[error("insufficient ROCKY: have {have}, need {need}")]
    Insufficient { have: u128, need: u128 },
    #[error("fee {0} bps exceeds the {MAX_FEE_BPS} bps ceiling")]
    FeeTooHigh(u64),
    #[error("airdrop of {0} recipients exceeds {MAX_AIRDROP}")]
    AirdropTooLarge(usize),
    #[error("insufficient shares: have {have}, need {need}")]
    InsufficientShares { have: u128, need: u128 },
    #[error("unstake cooldown: staked at {staked_at}, allowed from {allowed_at}, now {now}")]
    Cooldown { staked_at: u64, allowed_at: u64, now: u64 },
    #[error("pool is empty")]
    EmptyPool,
    #[error("cover {cover} exceeds the pool {pool}")]
    CoverExceedsPool { cover: u128, pool: u128 },
    #[error("premium {premium} below the minimum {min} for cover {cover}")]
    PremiumTooLow { premium: u128, min: u128, cover: u128 },
    #[error("policy window is not a plausible YYYYMMDD range")]
    BadWindow,
    #[error("no such policy")]
    NoPolicy,
    #[error("policy already claimed")]
    AlreadyClaimed,
    #[error("no fresh gauge reading for {0}")]
    GaugeMissing(&'static str),
    #[error("gauge reading {date} is outside the policy window {from}..={until}")]
    OutsideWindow { date: u32, from: u32, until: u32 },
    #[error("not triggered: K\u{2295} {k} ≤ p99 {p99} on {date}")]
    NotTriggered { k: i128, p99: i128, date: u32 },
    #[error("arithmetic overflow")]
    Overflow,
}

// ============ SLOT ENCODING ============
fn u128_of(v: [u8; 32]) -> u128 {
    u128::from_le_bytes(v[..16].try_into().unwrap())
}
fn slot_u128(state: &SigilState, slot: &SlotId) -> u128 {
    u128_of(state.contract_slot(&ROCKY_CONTRACT, slot))
}
fn enc_u128(v: u128) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&v.to_le_bytes());
    b
}
fn enc_u64(v: u64) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&v.to_le_bytes());
    b
}
fn slot_u64(state: &SigilState, slot: &SlotId) -> u64 {
    u64::from_le_bytes(state.contract_slot(&ROCKY_CONTRACT, slot)[..8].try_into().unwrap())
}
fn set(slot: SlotId, value: [u8; 32]) -> StateMutation {
    StateMutation::SetContractSlot { contract: ROCKY_CONTRACT, slot, value }
}

/// A policy as encoded in its slot: cover (16) ‖ from_date (4) ‖ until_date (4) ‖ claimed (1) ‖ paid? — paid amount lives in the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub holder: WalletId,
    #[serde(with = "u128_str")]
    pub cover: u128,
    pub from_date: u32,
    pub until_date: u32,
    pub claimed: bool,
}

fn enc_policy(p: &Policy) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..16].copy_from_slice(&p.cover.to_le_bytes());
    b[16..20].copy_from_slice(&p.from_date.to_le_bytes());
    b[20..24].copy_from_slice(&p.until_date.to_le_bytes());
    b[24] = p.claimed as u8;
    b
}

/// Read policy `n`, if it exists.
pub fn policy(state: &SigilState, n: u64) -> Option<Policy> {
    if n >= slot_u64(state, &SLOT_POLICY_COUNT) {
        return None;
    }
    let t = state.contract_slot(&ROCKY_CONTRACT, &policy_slot(n));
    let holder = state.contract_slot(&ROCKY_CONTRACT, &policy_holder_slot(n));
    Some(Policy {
        holder,
        cover: u128::from_le_bytes(t[..16].try_into().unwrap()),
        from_date: u32::from_le_bytes(t[16..20].try_into().unwrap()),
        until_date: u32::from_le_bytes(t[20..24].try_into().unwrap()),
        claimed: t[24] != 0,
    })
}

// ============ READERS ============
pub fn owner(state: &SigilState) -> Option<WalletId> {
    let o = state.contract_slot(&ROCKY_CONTRACT, &SLOT_OWNER);
    if o == [0u8; 32] { None } else { Some(o) }
}
pub fn bootstrapped(state: &SigilState) -> bool {
    slot_u64(state, &SLOT_BOOTSTRAPPED) != 0
}
pub fn paused(state: &SigilState) -> bool {
    slot_u64(state, &SLOT_PAUSED) != 0
}
pub fn fee_bps(state: &SigilState) -> u64 {
    slot_u64(state, &SLOT_FEE_BPS)
}
pub fn total_supply(state: &SigilState) -> u128 {
    slot_u128(state, &SLOT_TOTAL_SUPPLY)
}
pub fn reflect_index(state: &SigilState) -> u128 {
    slot_u128(state, &SLOT_REFLECT_INDEX)
}
pub fn reflect_pool(state: &SigilState) -> u128 {
    slot_u128(state, &SLOT_REFLECT_POOL)
}
pub fn pool_balance(state: &SigilState) -> u128 {
    slot_u128(state, &SLOT_POOL_BALANCE)
}
pub fn pool_shares(state: &SigilState) -> u128 {
    slot_u128(state, &SLOT_POOL_SHARES)
}
pub fn shares_of(state: &SigilState, w: &WalletId) -> u128 {
    slot_u128(state, &shares_slot(w))
}
pub fn policy_count(state: &SigilState) -> u64 {
    slot_u64(state, &SLOT_POLICY_COUNT)
}
/// Raw wallet balance (excludes reflection owed but not yet settled).
pub fn raw_balance(state: &SigilState, w: &WalletId) -> u128 {
    state.balance_of(w, &ROCKY)
}
/// Reflection owed to `w` since its last settle.
pub fn owed(state: &SigilState, w: &WalletId) -> u128 {
    let idx = reflect_index(state);
    let last = slot_u128(state, &holder_index_slot(w));
    let raw = raw_balance(state, w);
    if idx <= last || raw == 0 {
        return 0;
    }
    raw.saturating_mul(idx - last) / MAG
}
/// Balance including reflection owed — what a wallet would hold after `Settle`.
pub fn balance_of(state: &SigilState, w: &WalletId) -> u128 {
    raw_balance(state, w).saturating_add(owed(state, w).min(reflect_pool(state)))
}
/// Circulating = supply − pool − unsettled reflection pool (what the index divides by).
pub fn circulating(state: &SigilState) -> u128 {
    total_supply(state).saturating_sub(pool_balance(state)).saturating_sub(reflect_pool(state))
}
/// NAV of one share ×1e10 (glyph-scaled), 1e10 when the pool is empty.
pub fn nav_per_share_e10(state: &SigilState) -> u128 {
    let sh = pool_shares(state);
    if sh == 0 { ONE_ROCKY } else { pool_balance(state).saturating_mul(ONE_ROCKY) / sh }
}

/// One call's outcome: the mutations to commit and a domain-separated digest for the event.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub mutations: Vec<StateMutation>,
    pub result_hash: [u8; 32],
    /// Human line for logs / the API (never consensus).
    pub summary: String,
}

/// In-call working set: balances/slots read once, written once, so a call that touches the
/// same wallet twice (self-transfer, settle-then-move) stays consistent.
struct Ctx<'a> {
    state: &'a SigilState,
    bal: std::collections::BTreeMap<WalletId, u128>,
    slots: std::collections::BTreeMap<SlotId, [u8; 32]>,
}

impl<'a> Ctx<'a> {
    fn new(state: &'a SigilState) -> Self {
        Ctx { state, bal: Default::default(), slots: Default::default() }
    }
    fn bal(&mut self, w: &WalletId) -> u128 {
        *self.bal.entry(*w).or_insert_with(|| self.state.balance_of(w, &ROCKY))
    }
    fn set_bal(&mut self, w: &WalletId, v: u128) {
        self.bal.insert(*w, v);
    }
    fn slot(&mut self, s: &SlotId) -> [u8; 32] {
        *self.slots.entry(*s).or_insert_with(|| self.state.contract_slot(&ROCKY_CONTRACT, s))
    }
    fn u128(&mut self, s: &SlotId) -> u128 {
        u128_of(self.slot(s))
    }
    fn u64(&mut self, s: &SlotId) -> u64 {
        u64::from_le_bytes(self.slot(s)[..8].try_into().unwrap())
    }
    fn put(&mut self, s: SlotId, v: [u8; 32]) {
        self.slots.insert(s, v);
    }
    /// Settle reflection owed to `w` into its raw balance (pool-limited, exact to dust).
    fn settle(&mut self, w: &WalletId) -> u128 {
        let idx = self.u128(&SLOT_REFLECT_INDEX);
        let last = self.u128(&holder_index_slot(w));
        let raw = self.bal(w);
        let mut paid = 0;
        if idx > last && raw > 0 {
            let pool = self.u128(&SLOT_REFLECT_POOL);
            paid = (raw.saturating_mul(idx - last) / MAG).min(pool);
            self.set_bal(w, raw + paid);
            self.put(SLOT_REFLECT_POOL, enc_u128(pool - paid));
        }
        self.put(holder_index_slot(w), enc_u128(idx));
        paid
    }
    fn finish(self, mut extra: Vec<StateMutation>) -> Vec<StateMutation> {
        let mut m: Vec<StateMutation> = self
            .bal
            .into_iter()
            .map(|(wallet, amount)| StateMutation::SetBalance { wallet, token: ROCKY, amount })
            .collect();
        m.extend(self.slots.into_iter().map(|(slot, value)| set(slot, value)));
        m.append(&mut extra);
        m
    }
}

fn digest(name: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/rocky/result/v1/");
    h.update(name.as_bytes());
    for p in parts {
        h.update(p);
    }
    *h.finalize().as_bytes()
}

/// Apply one call. Pure: reads `state`, returns the mutations (or the refusal). `height` is
/// the block the call lands in; the gauge freshness and the unstake cooldown use it.
pub fn apply(state: &SigilState, caller: &WalletId, method: [u8; 4], call: &RockyCall, height: u64) -> Result<Outcome, RockyError> {
    if method != call.selector() {
        return Err(RockyError::SelectorMismatch);
    }
    let mut c = Ctx::new(state);
    let booted = slot_u64(state, &SLOT_BOOTSTRAPPED) != 0;
    if !matches!(call, RockyCall::Bootstrap { .. }) && !booted {
        return Err(RockyError::NotBootstrapped);
    }
    let own = owner(state);
    let is_owner = own.as_ref() == Some(caller);
    let name = call.name();
    match call {
        RockyCall::Bootstrap { owner: o, initial_supply } => {
            if booted { return Err(RockyError::AlreadyBootstrapped); }
            if state.master_wallet().as_ref() != Some(caller) { return Err(RockyError::NotMaster); }
            if *o == [0u8; 32] || *initial_supply == 0 { return Err(RockyError::Zero); }
            c.put(SLOT_OWNER, *o);
            c.put(SLOT_BOOTSTRAPPED, enc_u64(height.max(1)));
            c.put(SLOT_FEE_BPS, enc_u64(DEFAULT_FEE_BPS));
            c.put(SLOT_TOTAL_SUPPLY, enc_u128(*initial_supply));
            let have = c.bal(o);
            c.set_bal(o, have.checked_add(*initial_supply).ok_or(RockyError::Overflow)?);
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[o, &initial_supply.to_le_bytes()]), summary: format!("ROCKY bootstrapped: owner {} supply {initial_supply}", hex4(o)) })
        }
        RockyCall::Transfer { to, amount } => {
            if paused(state) { return Err(RockyError::Paused); }
            if *amount == 0 { return Err(RockyError::Zero); }
            c.settle(caller);
            c.settle(to);
            let have = c.bal(caller);
            if have < *amount { return Err(RockyError::Insufficient { have, need: *amount }); }
            let bps = c.u64(&SLOT_FEE_BPS) as u128;
            let fee = amount.saturating_mul(bps) / 10_000;
            let net = amount - fee;
            c.set_bal(caller, have - *amount);
            let tb = c.bal(to);
            c.set_bal(to, tb.checked_add(net).ok_or(RockyError::Overflow)?);
            if fee > 0 {
                // reflection: the fee joins the unsettled pool; the index rises by fee/circulating
                let pool = c.u128(&SLOT_REFLECT_POOL);
                let circ = circulating_ctx(&mut c).saturating_sub(fee).max(1);
                let idx = c.u128(&SLOT_REFLECT_INDEX);
                c.put(SLOT_REFLECT_POOL, enc_u128(pool.checked_add(fee).ok_or(RockyError::Overflow)?));
                c.put(SLOT_REFLECT_INDEX, enc_u128(idx.checked_add(fee.saturating_mul(MAG) / circ).ok_or(RockyError::Overflow)?));
            }
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[caller, to, &amount.to_le_bytes(), &fee.to_le_bytes()]), summary: format!("transfer {amount} ROCKY {}→{} (fee {fee})", hex4(caller), hex4(to)) })
        }
        RockyCall::Mint { to, amount } => {
            if !is_owner { return Err(RockyError::NotOwner); }
            if *amount == 0 { return Err(RockyError::Zero); }
            c.settle(to);
            let tb = c.bal(to);
            c.set_bal(to, tb.checked_add(*amount).ok_or(RockyError::Overflow)?);
            let ts = c.u128(&SLOT_TOTAL_SUPPLY);
            c.put(SLOT_TOTAL_SUPPLY, enc_u128(ts.checked_add(*amount).ok_or(RockyError::Overflow)?));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[to, &amount.to_le_bytes()]), summary: format!("mint {amount} ROCKY to {}", hex4(to)) })
        }
        RockyCall::Burn { amount } => {
            if *amount == 0 { return Err(RockyError::Zero); }
            c.settle(caller);
            let have = c.bal(caller);
            if have < *amount { return Err(RockyError::Insufficient { have, need: *amount }); }
            c.set_bal(caller, have - *amount);
            let ts = c.u128(&SLOT_TOTAL_SUPPLY);
            c.put(SLOT_TOTAL_SUPPLY, enc_u128(ts.saturating_sub(*amount)));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[caller, &amount.to_le_bytes()]), summary: format!("burn {amount} ROCKY from {}", hex4(caller)) })
        }
        RockyCall::SetFeeBps { bps } => {
            if !is_owner { return Err(RockyError::NotOwner); }
            if *bps > MAX_FEE_BPS { return Err(RockyError::FeeTooHigh(*bps)); }
            c.put(SLOT_FEE_BPS, enc_u64(*bps));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[&bps.to_le_bytes()]), summary: format!("reflection fee {bps} bps") })
        }
        RockyCall::Pause { on } => {
            if !is_owner { return Err(RockyError::NotOwner); }
            c.put(SLOT_PAUSED, enc_u64(*on as u64));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[&[*on as u8]]), summary: format!("paused={on}") })
        }
        RockyCall::Airdrop { recipients } => {
            if !is_owner { return Err(RockyError::NotOwner); }
            if paused(state) { return Err(RockyError::Paused); }
            if recipients.len() > MAX_AIRDROP { return Err(RockyError::AirdropTooLarge(recipients.len())); }
            if recipients.is_empty() || recipients.iter().any(|d| d.amount == 0) { return Err(RockyError::Zero); }
            c.settle(caller);
            let total: u128 = recipients.iter().try_fold(0u128, |acc, d| acc.checked_add(d.amount)).ok_or(RockyError::Overflow)?;
            let have = c.bal(caller);
            if have < total { return Err(RockyError::Insufficient { have, need: total }); }
            c.set_bal(caller, have - total);
            let mut h = blake3::Hasher::new();
            for d in recipients {
                c.settle(&d.to);
                let b = c.bal(&d.to);
                c.set_bal(&d.to, b.checked_add(d.amount).ok_or(RockyError::Overflow)?);
                h.update(&d.to);
                h.update(&d.amount.to_le_bytes());
            }
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[h.finalize().as_bytes(), &total.to_le_bytes()]), summary: format!("airdrop {total} ROCKY to {} wallets", recipients.len()) })
        }
        RockyCall::TransferOwnership { to } => {
            if !is_owner { return Err(RockyError::NotOwner); }
            if *to == [0u8; 32] { return Err(RockyError::Zero); }
            c.put(SLOT_OWNER, *to);
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[to]), summary: format!("owner → {}", hex4(to)) })
        }
        RockyCall::Stake { amount } => {
            if paused(state) { return Err(RockyError::Paused); }
            if *amount == 0 { return Err(RockyError::Zero); }
            c.settle(caller);
            let have = c.bal(caller);
            if have < *amount { return Err(RockyError::Insufficient { have, need: *amount }); }
            let pool = c.u128(&SLOT_POOL_BALANCE);
            let shares = c.u128(&SLOT_POOL_SHARES);
            // shares at current NAV; the first staker sets 1 share = 1 ROCKY
            let minted = if shares == 0 || pool == 0 { *amount } else { amount.saturating_mul(shares) / pool };
            if minted == 0 { return Err(RockyError::Zero); }
            c.set_bal(caller, have - *amount);
            c.put(SLOT_POOL_BALANCE, enc_u128(pool.checked_add(*amount).ok_or(RockyError::Overflow)?));
            c.put(SLOT_POOL_SHARES, enc_u128(shares.checked_add(minted).ok_or(RockyError::Overflow)?));
            let mine = c.u128(&shares_slot(caller));
            c.put(shares_slot(caller), enc_u128(mine.checked_add(minted).ok_or(RockyError::Overflow)?));
            c.put(staked_at_slot(caller), enc_u64(height));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[caller, &amount.to_le_bytes(), &minted.to_le_bytes()]), summary: format!("stake {amount} ROCKY → {minted} shares") })
        }
        RockyCall::Unstake { shares: n } => {
            if *n == 0 { return Err(RockyError::Zero); }
            let mine = c.u128(&shares_slot(caller));
            if mine < *n { return Err(RockyError::InsufficientShares { have: mine, need: *n }); }
            let staked_at = c.u64(&staked_at_slot(caller));
            let allowed_at = staked_at.saturating_add(UNSTAKE_COOLDOWN_BLOCKS);
            if height < allowed_at { return Err(RockyError::Cooldown { staked_at, allowed_at, now: height }); }
            let pool = c.u128(&SLOT_POOL_BALANCE);
            let shares = c.u128(&SLOT_POOL_SHARES);
            if shares == 0 { return Err(RockyError::EmptyPool); }
            let out = pool.saturating_mul(*n) / shares;
            c.settle(caller);
            let have = c.bal(caller);
            c.set_bal(caller, have.checked_add(out).ok_or(RockyError::Overflow)?);
            c.put(SLOT_POOL_BALANCE, enc_u128(pool - out));
            c.put(SLOT_POOL_SHARES, enc_u128(shares - *n));
            c.put(shares_slot(caller), enc_u128(mine - *n));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[caller, &n.to_le_bytes(), &out.to_le_bytes()]), summary: format!("unstake {n} shares → {out} ROCKY") })
        }
        RockyCall::BuyCover { premium, cover, from_date, until_date } => {
            if paused(state) { return Err(RockyError::Paused); }
            if *premium == 0 || *cover == 0 { return Err(RockyError::Zero); }
            if !sigil_oracle::plausible_reading_date(*from_date) || !sigil_oracle::plausible_reading_date(*until_date) || from_date > until_date {
                return Err(RockyError::BadWindow);
            }
            let pool = c.u128(&SLOT_POOL_BALANCE);
            if pool == 0 { return Err(RockyError::EmptyPool); }
            if cover.saturating_mul(10_000) > pool.saturating_mul(MAX_COVER_OF_POOL_BPS as u128) {
                return Err(RockyError::CoverExceedsPool { cover: *cover, pool });
            }
            let min = cover.saturating_mul(MIN_PREMIUM_BPS as u128) / 10_000;
            if *premium < min.max(1) { return Err(RockyError::PremiumTooLow { premium: *premium, min: min.max(1), cover: *cover }); }
            c.settle(caller);
            let have = c.bal(caller);
            if have < *premium { return Err(RockyError::Insufficient { have, need: *premium }); }
            let n = c.u64(&SLOT_POLICY_COUNT);
            if n >= MAX_POLICIES { return Err(RockyError::Overflow); }
            c.set_bal(caller, have - *premium);
            c.put(SLOT_POOL_BALANCE, enc_u128(pool.checked_add(*premium).ok_or(RockyError::Overflow)?));
            let p = Policy { holder: *caller, cover: *cover, from_date: *from_date, until_date: *until_date, claimed: false };
            c.put(policy_slot(n), enc_policy(&p));
            c.put(policy_holder_slot(n), *caller);
            c.put(SLOT_POLICY_COUNT, enc_u64(n + 1));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[caller, &n.to_le_bytes(), &premium.to_le_bytes(), &cover.to_le_bytes(), &from_date.to_le_bytes(), &until_date.to_le_bytes()]), summary: format!("policy #{n}: cover {cover} ROCKY for {from_date}..={until_date}, premium {premium}") })
        }
        RockyCall::Claim { policy: n } => {
            let p = policy(state, *n).ok_or(RockyError::NoPolicy)?;
            if p.claimed { return Err(RockyError::AlreadyClaimed); }
            let k = read_gauge(state, &feed_id(FEED_K)).filter(|g| gauge_is_fresh(g, height)).ok_or(RockyError::GaugeMissing(FEED_K))?;
            let p99 = read_gauge(state, &feed_id(FEED_P99)).filter(|g| gauge_is_fresh(g, height)).ok_or(RockyError::GaugeMissing(FEED_P99))?;
            if k.reading_date < p.from_date || k.reading_date > p.until_date {
                return Err(RockyError::OutsideWindow { date: k.reading_date, from: p.from_date, until: p.until_date });
            }
            if k.value_e6 <= p99.value_e6 {
                return Err(RockyError::NotTriggered { k: k.value_e6, p99: p99.value_e6, date: k.reading_date });
            }
            let pool = c.u128(&SLOT_POOL_BALANCE);
            let paid = p.cover.min(pool);
            c.settle(&p.holder);
            let hb = c.bal(&p.holder);
            c.set_bal(&p.holder, hb.checked_add(paid).ok_or(RockyError::Overflow)?);
            c.put(SLOT_POOL_BALANCE, enc_u128(pool - paid));
            c.put(policy_slot(*n), enc_policy(&Policy { claimed: true, ..p }));
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[&p.holder, &n.to_le_bytes(), &paid.to_le_bytes(), &k.value_e6.to_le_bytes(), &p99.value_e6.to_le_bytes(), &k.reading_date.to_le_bytes()]), summary: format!("policy #{n} claimed: K⊕ {} > p99 {} on {} → {paid} ROCKY to {}", k.value_e6, p99.value_e6, k.reading_date, hex4(&p.holder)) })
        }
        RockyCall::Settle => {
            let paid = c.settle(caller);
            Ok(Outcome { mutations: c.finish(vec![]), result_hash: digest(name, &[caller, &paid.to_le_bytes()]), summary: format!("settled {paid} ROCKY of reflection") })
        }
    }
}

fn circulating_ctx(c: &mut Ctx) -> u128 {
    let ts = c.u128(&SLOT_TOTAL_SUPPLY);
    let pool = c.u128(&SLOT_POOL_BALANCE);
    let rp = c.u128(&SLOT_REFLECT_POOL);
    ts.saturating_sub(pool).saturating_sub(rp)
}

fn hex4(w: &[u8; 32]) -> String {
    format!("{:02x}{:02x}{:02x}{:02x}…", w[0], w[1], w[2], w[3])
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::{commit_state_transition, StateTransition};

    const MASTER: WalletId = [0xAA; 32];
    const ROCKY_W: WalletId = [0x87; 32];
    const ALICE: WalletId = [0x11; 32];
    const BOB: WalletId = [0x22; 32];
    const CAROL: WalletId = [0x33; 32];

    fn fresh() -> SigilState {
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![StateMutation::SetMasterWallet { wallet: MASTER }] }, 0).unwrap();
        s
    }
    fn run(s: &mut SigilState, caller: WalletId, call: RockyCall, h: u64) -> Result<Outcome, RockyError> {
        let o = apply(s, &caller, call.selector(), &call, h)?;
        commit_state_transition(s, &StateTransition { at_height: h, mutations: o.mutations.clone() }, h).unwrap();
        Ok(o)
    }
    fn booted() -> SigilState {
        let mut s = fresh();
        run(&mut s, MASTER, RockyCall::Bootstrap { owner: ROCKY_W, initial_supply: 1_000_000 * ONE_ROCKY }, 2).unwrap();
        s
    }
    /// Conservation: every ROCKY is in a wallet, the cover pool, or the unsettled reflection pool.
    fn conserved(s: &SigilState, wallets: &[WalletId]) {
        let held: u128 = wallets.iter().map(|w| raw_balance(s, w)).sum();
        assert_eq!(held + pool_balance(s) + reflect_pool(s), total_supply(s), "ROCKY conservation");
    }

    #[test]
    fn bootstrap_is_master_only_and_once() {
        let mut s = fresh();
        assert_eq!(run(&mut s, ALICE, RockyCall::Bootstrap { owner: ROCKY_W, initial_supply: 1 }, 2).unwrap_err(), RockyError::NotMaster);
        assert_eq!(run(&mut s, ALICE, RockyCall::Transfer { to: BOB, amount: 1 }, 2).unwrap_err(), RockyError::NotBootstrapped);
        run(&mut s, MASTER, RockyCall::Bootstrap { owner: ROCKY_W, initial_supply: 5 * ONE_ROCKY }, 2).unwrap();
        assert_eq!(owner(&s), Some(ROCKY_W));
        assert_eq!(raw_balance(&s, &ROCKY_W), 5 * ONE_ROCKY);
        assert_eq!(total_supply(&s), 5 * ONE_ROCKY);
        assert_eq!(fee_bps(&s), DEFAULT_FEE_BPS);
        assert_eq!(run(&mut s, MASTER, RockyCall::Bootstrap { owner: ALICE, initial_supply: 1 }, 3).unwrap_err(), RockyError::AlreadyBootstrapped);
        // the selector must name the call
        assert_eq!(apply(&s, &ROCKY_W, selector("mint"), &RockyCall::Pause { on: true }, 3).unwrap_err(), RockyError::SelectorMismatch);
    }

    #[test]
    fn reflection_redistributes_the_fee_pro_rata_and_conserves_supply() {
        let mut s = booted();
        // owner hands Alice and Bob 100 each (fee 2 % → they receive 98, the 2s sit in the reflection pool)
        run(&mut s, ROCKY_W, RockyCall::Transfer { to: ALICE, amount: 100 * ONE_ROCKY }, 3).unwrap();
        assert_eq!(raw_balance(&s, &ALICE), 98 * ONE_ROCKY);
        assert_eq!(reflect_pool(&s), 2 * ONE_ROCKY, "the whole fee waits in the reflection pool until someone settles");
        // the owner still holds ~99.99 % of circulation, so at its next touch it is owed ~all of that fee
        let owner_owed_1 = owed(&s, &ROCKY_W);
        assert!(owner_owed_1 > 2 * ONE_ROCKY * 999 / 1000 && owner_owed_1 <= 2 * ONE_ROCKY, "{owner_owed_1}");
        run(&mut s, ROCKY_W, RockyCall::Transfer { to: BOB, amount: 100 * ONE_ROCKY }, 3).unwrap();
        // settled on touch: the pool holds (almost) only the SECOND fee now
        assert!(reflect_pool(&s) >= 2 * ONE_ROCKY && reflect_pool(&s) < 2 * ONE_ROCKY + ONE_ROCKY / 1000, "{}", reflect_pool(&s));
        conserved(&s, &[ROCKY_W, ALICE, BOB]);
        // Alice pays Carol 50 → fee 1; the index rises; Bob (who did nothing) is owed his share
        run(&mut s, ALICE, RockyCall::Transfer { to: CAROL, amount: 50 * ONE_ROCKY }, 4).unwrap();
        let bob_owed = owed(&s, &BOB);
        assert!(bob_owed > 0, "Bob accrues reflection without touching the contract");
        // everyone's owed share is proportional to their raw balance
        let alice_owed = owed(&s, &ALICE);
        let owner_owed = owed(&s, &ROCKY_W);
        assert!(owner_owed > bob_owed && bob_owed > alice_owed, "owner {owner_owed} > bob {bob_owed} > alice {alice_owed}");
        // settle everyone: the reflection pool drains (to dust) into balances; supply is conserved throughout
        for w in [ROCKY_W, ALICE, BOB, CAROL] {
            run(&mut s, w, RockyCall::Settle, 5).unwrap();
            conserved(&s, &[ROCKY_W, ALICE, BOB, CAROL]);
        }
        assert!(reflect_pool(&s) < 100, "only rounding dust remains: {}", reflect_pool(&s));
        assert_eq!(balance_of(&s, &BOB), raw_balance(&s, &BOB));
        // a second settle pays nothing more
        let o = run(&mut s, BOB, RockyCall::Settle, 6).unwrap();
        assert!(o.summary.starts_with("settled 0 "));
        // fee ceiling and owner-only knobs
        assert_eq!(run(&mut s, ALICE, RockyCall::SetFeeBps { bps: 100 }, 6).unwrap_err(), RockyError::NotOwner);
        assert_eq!(run(&mut s, ROCKY_W, RockyCall::SetFeeBps { bps: 1_001 }, 6).unwrap_err(), RockyError::FeeTooHigh(1_001));
        run(&mut s, ROCKY_W, RockyCall::SetFeeBps { bps: 0 }, 6).unwrap();
        let before = raw_balance(&s, &CAROL);
        run(&mut s, ALICE, RockyCall::Transfer { to: CAROL, amount: ONE_ROCKY }, 7).unwrap();
        assert_eq!(raw_balance(&s, &CAROL), before + ONE_ROCKY, "no fee at 0 bps");
    }

    #[test]
    fn mint_burn_pause_airdrop_and_ownership() {
        let mut s = booted();
        assert_eq!(run(&mut s, ALICE, RockyCall::Mint { to: ALICE, amount: 1 }, 3).unwrap_err(), RockyError::NotOwner);
        run(&mut s, ROCKY_W, RockyCall::Mint { to: ALICE, amount: 10 * ONE_ROCKY }, 3).unwrap();
        assert_eq!(total_supply(&s), 1_000_010 * ONE_ROCKY);
        run(&mut s, ALICE, RockyCall::Burn { amount: 4 * ONE_ROCKY }, 4).unwrap();
        assert_eq!(raw_balance(&s, &ALICE), 6 * ONE_ROCKY);
        assert_eq!(total_supply(&s), 1_000_006 * ONE_ROCKY);
        assert!(matches!(run(&mut s, ALICE, RockyCall::Burn { amount: 7 * ONE_ROCKY }, 4).unwrap_err(), RockyError::Insufficient { .. }));
        // airdrop (owner, ≤256, from the owner's balance, settles recipients)
        let drops: Vec<Drop> = (0..3u8).map(|i| Drop { to: [0x40 + i; 32], amount: (i as u128 + 1) * ONE_ROCKY }).collect();
        run(&mut s, ROCKY_W, RockyCall::Airdrop { recipients: drops.clone() }, 5).unwrap();
        assert_eq!(raw_balance(&s, &[0x42; 32]), 3 * ONE_ROCKY);
        assert_eq!(run(&mut s, ALICE, RockyCall::Airdrop { recipients: drops.clone() }, 5).unwrap_err(), RockyError::NotOwner);
        let too_many: Vec<Drop> = (0..257u32).map(|i| Drop { to: { let mut w = [0u8; 32]; w[..4].copy_from_slice(&i.to_le_bytes()); w }, amount: 1 }).collect();
        assert_eq!(run(&mut s, ROCKY_W, RockyCall::Airdrop { recipients: too_many }, 5).unwrap_err(), RockyError::AirdropTooLarge(257));
        conserved(&s, &[ROCKY_W, ALICE, [0x40; 32], [0x41; 32], [0x42; 32]]);
        // pause stops transfers/stakes/airdrops/cover but not burn or settle
        run(&mut s, ROCKY_W, RockyCall::Pause { on: true }, 6).unwrap();
        assert_eq!(run(&mut s, ALICE, RockyCall::Transfer { to: BOB, amount: 1 }, 6).unwrap_err(), RockyError::Paused);
        assert_eq!(run(&mut s, ALICE, RockyCall::Stake { amount: 1 }, 6).unwrap_err(), RockyError::Paused);
        run(&mut s, ALICE, RockyCall::Burn { amount: 1 }, 6).unwrap();
        run(&mut s, ROCKY_W, RockyCall::Pause { on: false }, 7).unwrap();
        // ownership moves once, then the old owner is a stranger
        run(&mut s, ROCKY_W, RockyCall::TransferOwnership { to: ALICE }, 8).unwrap();
        assert_eq!(owner(&s), Some(ALICE));
        assert_eq!(run(&mut s, ROCKY_W, RockyCall::Pause { on: true }, 8).unwrap_err(), RockyError::NotOwner);
        run(&mut s, ALICE, RockyCall::Mint { to: BOB, amount: 1 }, 8).unwrap();
    }

    #[test]
    fn staking_is_pool_shares_premiums_raise_nav_and_the_cooldown_binds() {
        let mut s = booted();
        run(&mut s, ROCKY_W, RockyCall::SetFeeBps { bps: 0 }, 3).unwrap(); // keep the arithmetic legible
        run(&mut s, ROCKY_W, RockyCall::Transfer { to: ALICE, amount: 1_000 * ONE_ROCKY }, 3).unwrap();
        run(&mut s, ROCKY_W, RockyCall::Transfer { to: BOB, amount: 1_000 * ONE_ROCKY }, 3).unwrap();
        run(&mut s, ALICE, RockyCall::Stake { amount: 100 * ONE_ROCKY }, 10).unwrap();
        assert_eq!(shares_of(&s, &ALICE), 100 * ONE_ROCKY, "first staker: 1 share = 1 ROCKY");
        assert_eq!(nav_per_share_e10(&s), ONE_ROCKY);
        // Bob buys cover: premium 10 into the pool → NAV rises for Alice
        run(&mut s, BOB, RockyCall::BuyCover { premium: 10 * ONE_ROCKY, cover: 50 * ONE_ROCKY, from_date: 20260901, until_date: 20260930 }, 11).unwrap();
        assert_eq!(pool_balance(&s), 110 * ONE_ROCKY);
        assert_eq!(nav_per_share_e10(&s), 110 * ONE_ROCKY / 100);
        // Carol stakes 110 now and gets 100 shares (NAV 1.1)
        run(&mut s, ROCKY_W, RockyCall::Transfer { to: CAROL, amount: 110 * ONE_ROCKY }, 11).unwrap();
        run(&mut s, CAROL, RockyCall::Stake { amount: 110 * ONE_ROCKY }, 12).unwrap();
        assert_eq!(shares_of(&s, &CAROL), 100 * ONE_ROCKY);
        // cooldown
        assert!(matches!(run(&mut s, ALICE, RockyCall::Unstake { shares: 1 }, 12).unwrap_err(), RockyError::Cooldown { .. }));
        let h = 10 + UNSTAKE_COOLDOWN_BLOCKS;
        run(&mut s, ALICE, RockyCall::Unstake { shares: 100 * ONE_ROCKY }, h).unwrap();
        assert_eq!(raw_balance(&s, &ALICE), 900 * ONE_ROCKY + 110 * ONE_ROCKY, "Alice leaves with her stake plus her half of the premium");
        assert_eq!(pool_balance(&s), 110 * ONE_ROCKY);
        conserved(&s, &[ROCKY_W, ALICE, BOB, CAROL]);
        // cover cannot exceed the pool; premium floor; window sanity
        assert!(matches!(run(&mut s, BOB, RockyCall::BuyCover { premium: 5 * ONE_ROCKY, cover: 200 * ONE_ROCKY, from_date: 20260901, until_date: 20260930 }, h).unwrap_err(), RockyError::CoverExceedsPool { .. }));
        assert!(matches!(run(&mut s, BOB, RockyCall::BuyCover { premium: 1, cover: 50 * ONE_ROCKY, from_date: 20260901, until_date: 20260930 }, h).unwrap_err(), RockyError::PremiumTooLow { .. }));
        assert_eq!(run(&mut s, BOB, RockyCall::BuyCover { premium: ONE_ROCKY, cover: 50 * ONE_ROCKY, from_date: 20260930, until_date: 20260901 }, h).unwrap_err(), RockyError::BadWindow);
    }

    fn push_gauge(s: &mut SigilState, name: &str, value_e6: i128, date: u32, h: u64) {
        let m = sigil_oracle::gauge_mutations(feed_id(name), value_e6, date, h, 1, [0xA7; 32]);
        commit_state_transition(s, &StateTransition { at_height: h, mutations: m.to_vec() }, h).unwrap();
    }

    #[test]
    fn a_claim_pays_only_when_the_committed_k_exceeds_the_committed_p99_inside_the_window() {
        let mut s = booted();
        run(&mut s, ROCKY_W, RockyCall::SetFeeBps { bps: 0 }, 3).unwrap();
        run(&mut s, ROCKY_W, RockyCall::Transfer { to: ALICE, amount: 1_000 * ONE_ROCKY }, 3).unwrap();
        run(&mut s, ROCKY_W, RockyCall::Transfer { to: BOB, amount: 100 * ONE_ROCKY }, 3).unwrap();
        run(&mut s, ALICE, RockyCall::Stake { amount: 500 * ONE_ROCKY }, 10).unwrap();
        run(&mut s, BOB, RockyCall::BuyCover { premium: 2 * ONE_ROCKY, cover: 200 * ONE_ROCKY, from_date: 20260910, until_date: 20260920 }, 11).unwrap();
        assert_eq!(policy_count(&s), 1);
        // no gauge on chain yet
        assert_eq!(run(&mut s, BOB, RockyCall::Claim { policy: 0 }, 12).unwrap_err(), RockyError::GaugeMissing(FEED_K));
        // K below p99: not triggered
        push_gauge(&mut s, FEED_P99, 3_120_000, 20260913, 12);
        push_gauge(&mut s, FEED_K, 2_808_974, 20260913, 12);
        assert!(matches!(run(&mut s, BOB, RockyCall::Claim { policy: 0 }, 13).unwrap_err(), RockyError::NotTriggered { .. }));
        // K above p99 but outside the window
        push_gauge(&mut s, FEED_K, 3_400_000, 20260925, 13);
        assert!(matches!(run(&mut s, BOB, RockyCall::Claim { policy: 0 }, 14).unwrap_err(), RockyError::OutsideWindow { date: 20260925, .. }));
        // K above p99 inside the window: anyone can trigger, the HOLDER is paid, once
        push_gauge(&mut s, FEED_K, 3_400_000, 20260918, 14);
        let before = raw_balance(&s, &BOB);
        let o = run(&mut s, CAROL, RockyCall::Claim { policy: 0 }, 15).unwrap();
        assert!(o.summary.contains("claimed"));
        assert_eq!(raw_balance(&s, &BOB), before + 200 * ONE_ROCKY);
        assert_eq!(pool_balance(&s), 302 * ONE_ROCKY, "500 staked + 2 premium − 200 paid");
        assert_eq!(run(&mut s, BOB, RockyCall::Claim { policy: 0 }, 16).unwrap_err(), RockyError::AlreadyClaimed);
        assert_eq!(run(&mut s, BOB, RockyCall::Claim { policy: 7 }, 16).unwrap_err(), RockyError::NoPolicy);
        conserved(&s, &[ROCKY_W, ALICE, BOB, CAROL]);
        // a stale gauge (older than MAX_GAUGE_AGE_BLOCKS) cannot trigger
        run(&mut s, BOB, RockyCall::BuyCover { premium: 2 * ONE_ROCKY, cover: 10 * ONE_ROCKY, from_date: 20260910, until_date: 20260920 }, 16).unwrap();
        assert_eq!(run(&mut s, BOB, RockyCall::Claim { policy: 1 }, 16 + sigil_oracle::MAX_GAUGE_AGE_BLOCKS + 10).unwrap_err(), RockyError::GaugeMissing(FEED_K));
        // a thin pool pays min(cover, pool) — visible before you buy
        push_gauge(&mut s, FEED_K, 3_400_000, 20260918, 17);
        let mut s2 = s.clone();
        commit_state_transition(&mut s2, &StateTransition { at_height: 17, mutations: vec![set(SLOT_POOL_BALANCE, enc_u128(3 * ONE_ROCKY))] }, 17).unwrap();
        let o = run(&mut s2, BOB, RockyCall::Claim { policy: 1 }, 18).unwrap();
        assert!(o.summary.contains(&format!("{} ROCKY", 3 * ONE_ROCKY)));
        assert_eq!(pool_balance(&s2), 0);
    }

    #[test]
    fn calls_round_trip_as_json_and_selectors_are_stable() {
        let c = RockyCall::BuyCover { premium: 5, cover: 1 << 100, from_date: 20260910, until_date: 20260920 };
        let back = RockyCall::decode(&c.encode()).unwrap();
        assert_eq!(back, c);
        assert!(String::from_utf8(c.encode()).unwrap().contains("\"call\":\"buy_cover\""));
        assert_eq!(c.selector(), selector("buy_cover"));
        assert_ne!(selector("stake"), selector("unstake"));
        assert!(RockyCall::decode(b"{\"call\":\"nope\"}").is_err());
    }
}
