//! sigil-tx — the transaction layer.
//!
//! Closes the data-flow gap between wallets/RPC and the Track C state
//! machine. Every `SigilTx` variant maps deterministically to a
//! `(Vec<StateMutation>, Vec<SigilEvent>)` pair via [`apply_tx`]. The node
//! then folds those into a single `StateTransition`, hands it to
//! `sigil_state::commit_state_transition`, and the four state roots
//! advance.
//!
//! What this crate is NOT:
//! - **It does not verify signatures.** That's `flux-eternal-cypher` in P1
//!   (dispatches to `flux-sqisign` by default, `flux-sigil-dilithium` for
//!   the crypto-agile fallback). [`SignedTx::verify_signature`] returns a
//!   `NotImplemented` stub until those crates port.
//! - **It does not pay fees to validators.** Fee distribution is consensus
//!   work (Track A). Here, fees just debit the sender and the rest is
//!   declared in the corresponding event.
//! - **It does not enforce nonce uniqueness across blocks.** The mempool +
//!   block builder do that in P2 — for Phase 0, `SigilState` doesn't track
//!   per-account nonces yet.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// Wire-format adapter for `u128`. SIGIL tx fixtures, gossipsub frames, and
/// future RPC payloads all carry amounts as decimal strings (`"1000000"`),
/// not JSON numbers — because serde_json's derive-generated `deserialize_u128`
/// returns "u128 is not supported" with the default feature set, and even with
/// `arbitrary_precision` enabled the round-trip is fragile. Strings dodge the
/// problem entirely and match how Ethereum, Quillon, and basically every
/// other chain encodes large amounts on the wire.
///
/// Apply with `#[serde(with = "u128_str")]` on each `u128` field.
pub mod u128_str {
    use serde::{Deserialize, Deserializer, Serializer};
    /// Serialize a `u128` as its decimal string form.
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    /// Deserialize from a decimal string. Hex/0x is rejected — keeps the
    /// surface narrow.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

use sigil_dex::{Pool as DexPool, SwapDirection};
use sigil_events::SigilEvent;
use sigil_header::{PubKeyBytes, SigScheme, SignatureBytes, ValidatorId};
use sigil_state::{
    PoolState, SigilState, StateMutation, StateTransition, TokenId, WalletId, ContractId,
    PoolId,
};

/// Lift a `sigil_state::PoolState` into the `sigil_dex::Pool` snapshot the
/// AMM math wants. Both sides carry the same numbers; the only difference is
/// `sigil_dex::Pool` doesn't know about token identifiers — those live in
/// `sigil_state::PoolState` and are recombined when we write back via
/// [`pool_state_from_dex`].
fn dex_pool_from_state(p: &PoolState) -> DexPool {
    DexPool {
        reserve_a:      p.reserve_a,
        reserve_b:      p.reserve_b,
        total_shares:   p.lp_shares,
        fee_bps:        p.fee_bps,
        accrued_fees_a: 0,
        accrued_fees_b: 0,
    }
}

/// Fold a dex Pool snapshot back into the persisted PoolState, carrying the
/// pre-existing token IDs and merging the accrued-fee delta into the running
/// total. The two counters in `sigil_dex::Pool` (per-side) get summed into
/// `sigil_state::PoolState::accrued_fees` (running total across both sides).
fn pool_state_from_dex(
    prev: &PoolState,
    after: &DexPool,
) -> Result<PoolState, TxApplyError> {
    let fees_delta = after
        .accrued_fees_a
        .checked_add(after.accrued_fees_b)
        .ok_or(TxApplyError::Overflow)?;
    Ok(PoolState {
        token_a:      prev.token_a,
        token_b:      prev.token_b,
        reserve_a:    after.reserve_a,
        reserve_b:    after.reserve_b,
        lp_shares:    after.total_shares,
        fee_bps:      after.fee_bps,
        accrued_fees: prev.accrued_fees
            .checked_add(fees_delta)
            .ok_or(TxApplyError::Overflow)?,
    })
}

/// All transaction kinds SIGIL accepts at v0. Every variant naturally maps
/// to exactly one `SigilEvent` kind (sometimes two — `Send` produces
/// `Send` on the sender side and `Receive` on the recipient side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SigilTx {
    /// PV-1: publish this wallet's shielded public key.
    ///
    /// One transparent, wallet-signed transaction that redirects future block rewards for
    /// this wallet into the shielded pool. Wallet-authenticated (unlike a shielded send)
    /// because it names a wallet and only its owner may redirect its income.
    ///
    /// The key is public by nature — `compress2` is one-way, so publishing it never
    /// exposes the spend key. It does reveal an intent to receive privately, which is
    /// unavoidable: someone has to be told where to pay.
    RegisterShieldedAddress {
        wallet: WalletId,
        pk_shield: [u8; 32],
        /// X25519 note-delivery key (`pk_enc`) — lets a sender seal a ciphertext this
        /// wallet can trial-decrypt. `#[serde(default)]` so an already-broadcast
        /// registration (pk_shield only) still decodes.
        #[serde(default)]
        pk_encrypt: Option<[u8; 32]>,
        /// SQIsign L5 public key (129 bytes), upgrading this wallet's RAMP authorization
        /// to post-quantum. `#[serde(default)]` so every already-broadcast registration
        /// still decodes, and so a client that has never heard of this field keeps working.
        ///
        /// The API layer refuses to populate this without a proof-of-possession signature
        /// by the key itself (`shielded::verify_sqi_possession`) — an unproven key here
        /// would be a wallet-hijack primitive, not a security upgrade.
        #[serde(default)]
        pk_sqi: Option<Vec<u8>>,
        #[serde(with = "sigil_state::u128_str")]
        fee: u128,
    },

    /// PV-1: move value from a transparent wallet into the shielded pool.
    ///
    /// The only public numbers are the depositor and the amount — necessarily so, since
    /// the value is leaving a transparent balance. What stays private is everything
    /// afterwards: once the note is in the pool, subsequent spends reveal neither amounts
    /// nor which note is being consumed.
    Shield {
        from: WalletId,
        #[serde(with = "sigil_state::u128_str")]
        amount: u128,
        /// `compress2(amount, blinding)` — the note the depositor can later spend.
        cm: [u8; 32],
        #[serde(with = "sigil_state::u128_str")]
        fee: u128,
    },

    /// PV-1: a shielded-to-shielded transfer. Amounts and linkage stay private.
    ///
    /// Carries no `from`, no `to`, and no amount — that is the point. The STARK proves
    /// the spender owned a note in the anchored tree, that its value equals the fee plus
    /// the outputs, and that each output commitment is bound to an in-range share of that
    /// value. The chokepoint verifies the proof before any state moves.
    ShieldedSend {
        anchor: [u8; 32],
        nullifier: [u8; 32],
        /// Nullifiers for inputs BEYOND the first. Empty for a 1-input spend (v4/v5); one
        /// entry for a 2-input spend (v6).
        ///
        /// Additive rather than turning `nullifier` into a `Vec`, so every transaction
        /// already recorded in the chain log keeps deserialising — `#[serde(default)]`
        /// gives an empty vector for the old shape. The cost is that a careless reader can
        /// see `nullifier` and miss the rest, so nothing reads it directly: use
        /// [`SigilTx::shielded_send_nullifiers`], and see the atomicity note there.
        #[serde(default)]
        extra_nullifiers: Vec<[u8; 32]>,
        cm_outs: Vec<[u8; 32]>,
        #[serde(with = "sigil_state::u128_str")]
        fee: u128,
        proof: Vec<u8>,
        /// Per-output delivery ciphertext, same order as `cm_outs`. Empty means the
        /// sender attached none (e.g. an all-self-change spend); when non-empty it must
        /// match `cm_outs` in length — see the apply logic. Not a STARK public input.
        #[serde(default)]
        note_ciphertexts: Vec<Option<String>>,
    },

    /// PV-1: move value out of the shielded pool to a transparent wallet.
    ///
    /// Proof-carrying, exactly like [`SigilTx::ShieldedSend`] — the withdrawn `amount`
    /// sits in the circuit's public-value slot, so the STARK proves the caller owned a
    /// note worth it. Without that, naming a nullifier would be enough to drain the pool.
    Unshield {
        to: WalletId,
        #[serde(with = "sigil_state::u128_str")]
        amount: u128,
        anchor: [u8; 32],
        nullifier: [u8; 32],
        cm_outs: Vec<[u8; 32]>,
        proof: Vec<u8>,
        #[serde(with = "sigil_state::u128_str")]
        fee: u128,
    },

    /// Move tokens from one wallet to another.
    Send {
        /// Sender wallet.
        from: WalletId,
        /// Recipient wallet.
        to: WalletId,
        /// Amount in the token's base units.
        #[serde(with = "u128_str")]
        amount: u128,
        /// Token to send. All-zero = native SIGIL.
        token: TokenId,
        /// Fee paid in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Swap one token for another via a DEX pool.
    Swap {
        /// Wallet executing the swap.
        from: WalletId,
        /// Pool to route through.
        pool: PoolId,
        /// Input token.
        in_token: TokenId,
        /// Input amount.
        #[serde(with = "u128_str")]
        in_amt: u128,
        /// Minimum acceptable output (slippage protection).
        #[serde(with = "u128_str")]
        min_out: u128,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Deposit liquidity into a pool, receiving LP shares.
    ///
    /// On first deposit (empty pool), `token_a` / `token_b` / `fee_bps` are
    /// used to create the pool. On subsequent deposits they are verified
    /// against the existing pool — mismatch → [`TxApplyError::PoolMismatch`].
    /// The strict policy matches the P5 scope doc (open question #1 — vote
    /// "implicit for P5, governance lift in P7+"); a future patch can relax
    /// the verify-on-subsequent rule once a `CreatePoolTx` exists.
    LpDeposit {
        /// Provider wallet.
        from: WalletId,
        /// Target pool.
        pool: PoolId,
        /// Token-A identifier — must equal pool's token_a, or define it on
        /// first deposit.
        token_a: TokenId,
        /// Token-B identifier.
        token_b: TokenId,
        /// Amount of token A deposited.
        #[serde(with = "u128_str")]
        amt_a: u128,
        /// Amount of token B deposited.
        #[serde(with = "u128_str")]
        amt_b: u128,
        /// Per-swap fee in basis points — locked on first deposit, verified
        /// thereafter.
        fee_bps: u16,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Burn LP shares and withdraw underlying.
    LpWithdraw {
        /// Provider wallet.
        from: WalletId,
        /// Pool to withdraw from.
        pool: PoolId,
        /// LP shares to burn.
        #[serde(with = "u128_str")]
        shares: u128,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Mint USDS by locking native SIGIL collateral into `sigil_usds::VAULT`
    /// at the committed oracle price (see `sigil-usds` for the buffer +
    /// protocol-fee math this routes through).
    UsdsMint {
        /// Wallet locking collateral.
        from: WalletId,
        /// SIGIL to lock.
        #[serde(with = "u128_str")]
        sigil_amount: u128,
        /// Fee in native SIGIL (on top of the locked collateral).
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Burn USDS, release the underlying SIGIL collateral at the current
    /// oracle price, minus the protocol fee.
    UsdsRedeem {
        /// Wallet redeeming.
        from: WalletId,
        /// USDS to burn.
        #[serde(with = "u128_str")]
        usds_amount: u128,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Invoke a VM contract method.
    ContractCall {
        /// Caller wallet.
        from: WalletId,
        /// Contract to call.
        contract: ContractId,
        /// 4-byte method selector.
        method: [u8; 4],
        /// Calldata (method-specific encoding).
        calldata: Vec<u8>,
        /// Gas limit.
        gas_limit: u64,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Deploy a new VM contract.
    ContractDeploy {
        /// Deployer wallet.
        from: WalletId,
        /// Contract bytecode.
        bytecode: Vec<u8>,
        /// Constructor calldata.
        constructor_args: Vec<u8>,
        /// Gas limit.
        gas_limit: u64,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Mint a new fungible token (Quillon-compatible `deploy_token` shape).
    TokenDeploy {
        /// Creator wallet, receives the initial supply.
        creator: WalletId,
        /// Display ticker (case-sensitive).
        ticker: String,
        /// Decimal places.
        decimals: u8,
        /// Initial supply minted to the creator.
        #[serde(with = "u128_str")]
        initial_supply: u128,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Stake to join the validator set.
    ValidatorJoin {
        /// Validator joining.
        validator: ValidatorId,
        /// Stake amount in native SIGIL (debited from the validator's wallet
        /// — same 32 bytes serve both ID and address).
        #[serde(with = "u128_str")]
        stake: u128,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// Exit the validator set, get the stake back.
    ValidatorLeave {
        /// Validator exiting.
        validator: ValidatorId,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// T6 — create a bounded, expiring agent spend-authority
    /// (`sigil_bank::mandate::Mandate`). No balance moves on create; this
    /// grants permission, not funds.
    MandateCreate {
        /// Mandate id — assigned ONCE by whoever builds this tx (e.g.
        /// blake3(agent‖purpose‖expires_ts‖nonce)) and carried verbatim.
        /// MUST NOT be re-derived at apply time from local wall-clock or
        /// local book length — either would diverge across nodes replaying
        /// the same tx at different times / with different prior state.
        id: String,
        /// The agent wallet this mandate authorizes. Also the fee payer —
        /// an agent requests its own bounded authority.
        agent: WalletId,
        /// Spend ceiling, native SIGIL base units.
        #[serde(with = "u128_str")]
        max_amount: u128,
        /// Audit-trail purpose string.
        purpose: String,
        /// Unix-seconds absolute creation time, computed once by the tx
        /// builder (same determinism reason as `expires_ts`).
        created_ts: u64,
        /// Unix-seconds absolute expiry, computed once by the tx builder.
        expires_ts: u64,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// T6 — close an existing mandate. Only the mandate's own agent may
    /// close it (checked at apply time against the replayed book).
    MandateClose {
        /// Mandate id being closed.
        id: String,
        /// The agent closing it.
        agent: WalletId,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// T6 — a council member files a treasury-transfer proposal (auto-
    /// approves it as approval #1). No balance moves here.
    BankPropose {
        /// Proposal id — assigned once by the tx builder, carried verbatim.
        id: String,
        /// Treasury wallet the transfer would debit.
        from: WalletId,
        /// Recipient wallet.
        to: WalletId,
        /// Token to transfer. All-zero = native SIGIL.
        token: TokenId,
        /// Amount in the token's base units.
        #[serde(with = "u128_str")]
        amount: u128,
        /// The proposing council member — also the fee payer (counts as
        /// approval #1, same as `Council::propose` already does).
        proposer: WalletId,
        /// Unix-seconds absolute creation time, computed once by the tx
        /// builder.
        created_ts: u64,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// T6 — a council member approves an existing proposal. No balance
    /// moves here either — whether this reaches threshold is a caller-side
    /// question (the caller holds the replayed Council); if it does, the
    /// caller separately builds a `BankExecute`.
    BankApprove {
        /// Proposal id being approved.
        id: String,
        /// The approving council member.
        approver: WalletId,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// T6 — execute a treasury transfer whose proposal just reached the
    /// council's approval threshold. Unlike Propose/Approve, this DOES move
    /// money — validated the same way `Send` is (balance-sufficiency
    /// checked here, aliasing-safe).
    BankExecute {
        /// Proposal id that reached threshold.
        id: String,
        /// Treasury wallet to debit.
        from: WalletId,
        /// Recipient wallet to credit.
        to: WalletId,
        /// Token to move. All-zero = native SIGIL.
        token: TokenId,
        /// Amount to move, in the token's base units.
        #[serde(with = "u128_str")]
        amount: u128,
        /// Whichever council member's approval tipped this proposal over
        /// threshold — a treasury-mandated transfer has no single natural
        /// signer, so this is the closest honest fee payer.
        executor: WalletId,
        /// Fee in native SIGIL.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// SIGIL-Nation — attest a wallet as a citizen in the borger registry.
    /// Consensus requires the signer to be the genesis-committed **master
    /// wallet** (the only wallet with real keys that genesis names — the
    /// legacy `BORGER_AUTHORITY` placeholder has no keyholder). Active from
    /// `sigil_bank::welfare::WELFARE_FROM_HEIGHT`.
    CitizenAttest {
        /// The attesting authority — must equal the chain's master wallet.
        authority: WalletId,
        /// The wallet being recognized as a citizen.
        citizen: WalletId,
        /// Hash of the citizen's civil identity (e.g. BLAKE3 of a CPR
        /// number). The raw identity NEVER goes on chain — only this hash.
        cpr_hash: [u8; 32],
        /// Fee in native SIGIL, paid by the authority.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// SIGIL-Nation — an attested citizen claims the periodic welfare
    /// stipend, at most once per `WELFARE_CLAIM_INTERVAL_BLOCKS`.
    ///
    /// **Paid in sUSD** (operator ruling 2026-08-31): the treasury's SIGIL is
    /// locked into the USDS vault as collateral (105% buffer, oracle-priced)
    /// and exactly `sigil_bank::welfare::WELFARE_STIPEND_USD_E8` of freshly
    /// minted USDS lands in the citizen's wallet — the stipend is a promise
    /// about purchasing power, not a bet on SIGIL's price. The fee burns
    /// FROM THE TREASURY (bounded by `WELFARE_STIPEND_GLYPHS`), so a citizen
    /// with a zero balance can always claim — welfare that requires money to
    /// receive isn't welfare. No oracle price or an underfunded treasury
    /// refuses the claim: fail closed, never an unbacked payment.
    WelfareClaim {
        /// The claiming citizen — also the signer.
        citizen: WalletId,
        /// Fee in native SIGIL, burned from the welfare treasury.
        #[serde(with = "u128_str")]
        fee: u128,
    },
    /// SIGIL-Nation — push the SIGIL/USD oracle price (USD×1e8 per whole
    /// SIGIL). Signer must be the state-committed **master wallet** — the
    /// genesis `ORACLE_AUTHORITY` placeholder (`[0x0A;32]`) has no keyholder,
    /// same reasoning as `CitizenAttest`. Height-gated with the nation txs:
    /// the oracle's consumer is the USDS welfare payout, and without a price
    /// every claim refuses (fail closed) — so pushing the price is part of
    /// operating the nation, not an optional extra.
    OraclePush {
        /// The pushing authority — must equal the chain's master wallet.
        authority: WalletId,
        /// Price in USD×1e8 per whole SIGIL. Must be non-zero.
        #[serde(with = "u128_str")]
        price_usd_e8: u128,
        /// Fee in native SIGIL, paid by the authority.
        #[serde(with = "u128_str")]
        fee: u128,
    },
}

/// Compact tag for indexing — matches [`SigilEvent::tag`] convention. The
/// tag is dense in the order variants are declared above; do NOT reorder.
pub type TxTag = u8;

impl SigilTx {
    /// Stable tag for the `txs_by_kind` flux-db CF (lands with storage P3).
    pub fn tag(&self) -> TxTag {
        match self {
            SigilTx::Send            { .. } => 0,
            SigilTx::Swap            { .. } => 1,
            SigilTx::LpDeposit       { .. } => 2,
            SigilTx::LpWithdraw      { .. } => 3,
            SigilTx::ContractCall    { .. } => 4,
            SigilTx::ContractDeploy  { .. } => 5,
            SigilTx::TokenDeploy     { .. } => 6,
            SigilTx::ValidatorJoin   { .. } => 7,
            SigilTx::ValidatorLeave  { .. } => 8,
            SigilTx::UsdsMint        { .. } => 9,
            SigilTx::UsdsRedeem      { .. } => 10,
            SigilTx::MandateCreate   { .. } => 11,
            SigilTx::MandateClose    { .. } => 12,
            SigilTx::BankPropose     { .. } => 13,
            SigilTx::BankApprove     { .. } => 14,
            SigilTx::BankExecute     { .. } => 15,
            SigilTx::RegisterShieldedAddress { .. } => 19,
            SigilTx::Shield          { .. } => 16,
            SigilTx::ShieldedSend    { .. } => 17,
            SigilTx::Unshield        { .. } => 18,
            SigilTx::CitizenAttest   { .. } => 20,
            SigilTx::WelfareClaim    { .. } => 21,
            SigilTx::OraclePush      { .. } => 22,
        }
    }

    /// Fee declared by the tx. Centralized here so the mempool can prioritize
    /// without case-matching the enum at every call site.
    pub fn fee(&self) -> u128 {
        match self {
            SigilTx::RegisterShieldedAddress { fee, .. } |
            SigilTx::Shield          { fee, .. } |
            SigilTx::ShieldedSend    { fee, .. } |
            SigilTx::Unshield        { fee, .. } |
            SigilTx::Send            { fee, .. } |
            SigilTx::Swap            { fee, .. } |
            SigilTx::LpDeposit       { fee, .. } |
            SigilTx::LpWithdraw      { fee, .. } |
            SigilTx::ContractCall    { fee, .. } |
            SigilTx::ContractDeploy  { fee, .. } |
            SigilTx::TokenDeploy     { fee, .. } |
            SigilTx::ValidatorJoin   { fee, .. } |
            SigilTx::ValidatorLeave  { fee, .. } |
            SigilTx::UsdsMint        { fee, .. } |
            SigilTx::UsdsRedeem      { fee, .. } |
            SigilTx::MandateCreate   { fee, .. } |
            SigilTx::MandateClose    { fee, .. } |
            SigilTx::BankPropose     { fee, .. } |
            SigilTx::BankApprove     { fee, .. } |
            SigilTx::BankExecute     { fee, .. } |
            SigilTx::CitizenAttest   { fee, .. } |
            SigilTx::WelfareClaim    { fee, .. } |
            SigilTx::OraclePush      { fee, .. } => *fee,
        }
    }

    /// Wallet that pays the fee. Always the natural author of the tx.
    /// # Shielded transactions
    ///
    /// [`SigilTx::ShieldedSend`] and [`SigilTx::Unshield`] have NO transparent payer —
    /// that is the entire point of them. Their fee is paid out of the shielded value the
    /// STARK proves the spender owned, and authorization comes from that proof rather
    /// than from a wallet signature. They report the null wallet, and
    /// [`SignedTx::precheck`] exempts them from the signer-equals-payer rule accordingly.
    /// Forcing a real wallet here would reintroduce exactly the linkage the shielded pool
    /// exists to break.
    pub fn fee_payer(&self) -> WalletId {
        match self {
            SigilTx::RegisterShieldedAddress { wallet, .. } => *wallet,
            SigilTx::Shield { from, .. } => *from,
            SigilTx::ShieldedSend { .. } | SigilTx::Unshield { .. } => [0u8; 32],
            SigilTx::Send         { from, .. } => *from,
            SigilTx::Swap         { from, .. } => *from,
            SigilTx::LpDeposit    { from, .. } => *from,
            SigilTx::LpWithdraw   { from, .. } => *from,
            SigilTx::ContractCall { from, .. } => *from,
            SigilTx::ContractDeploy { from, .. } => *from,
            SigilTx::TokenDeploy  { creator, .. } => *creator,
            SigilTx::ValidatorJoin { validator, .. } => *validator,
            SigilTx::ValidatorLeave { validator, .. } => *validator,
            SigilTx::UsdsMint { from, .. } => *from,
            SigilTx::UsdsRedeem { from, .. } => *from,
            SigilTx::MandateCreate { agent, .. } => *agent,
            SigilTx::MandateClose { agent, .. } => *agent,
            SigilTx::BankPropose { proposer, .. } => *proposer,
            SigilTx::BankApprove { approver, .. } => *approver,
            SigilTx::BankExecute { executor, .. } => *executor,
            SigilTx::CitizenAttest { authority, .. } => *authority,
            SigilTx::WelfareClaim { citizen, .. } => *citizen,
            SigilTx::OraclePush { authority, .. } => *authority,
        }
    }

    /// Every nullifier a shielded spend reveals, first one included.
    ///
    /// THE POINT OF THIS EXISTING: `extra_nullifiers` is additive, so `nullifier` alone
    /// still compiles and still looks complete. A path that reads it and records only that
    /// one accepts a 2-input spend while burning a single note — the other input's note
    /// stays spendable, which is a double-spend by omission. Read them through here.
    ///
    /// Returns `None` for a transaction that is not a shielded spend.
    pub fn shielded_send_nullifiers(&self) -> Option<Vec<[u8; 32]>> {
        match self {
            SigilTx::ShieldedSend { nullifier, extra_nullifiers, .. } => {
                let mut v = Vec::with_capacity(1 + extra_nullifiers.len());
                v.push(*nullifier);
                v.extend_from_slice(extra_nullifiers);
                Some(v)
            }
            _ => None,
        }
    }

    /// Deterministic bytes for signing — canonical JSON in P0, swaps to
    /// bincode with [`sigil_events`] in P3.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// BLAKE3 of the encoded bytes — the tx hash. Stable identifier across
    /// mempool, gossip, indexers, RPC.
    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(&self.encode()).as_bytes()
    }
}

// ── Signed wrapper ──────────────────────────────────────────────────────────

/// A tx as it actually flows on the wire: the inner intent plus the
/// signature material the producer needs to verify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTx {
    /// The intent.
    pub tx: SigilTx,
    /// Producer's account / pubkey (32 bytes, same as `WalletId` / `ValidatorId`).
    pub from_pubkey: WalletId,
    /// Per-account nonce — duplicate-spend protection. Mempool rejects
    /// duplicates; consensus checks ordering. Not enforced in this crate.
    pub nonce: u64,
    /// Signature scheme used. Defaults to [`SigScheme::SqiSign5`] per the
    /// header lock.
    pub sig_scheme: SigScheme,
    /// Signature bytes — length must match `sig_scheme.expected_sig_len()`.
    pub sig: SignatureBytes,
    /// Full scheme public key (129 B for SqiSign5). Carried because a 32-byte
    /// `from_pubkey` WalletId can't hold it. The verifier checks
    /// `len == sig_scheme.expected_pubkey_len()` AND the account binding
    /// `from_pubkey == BLAKE3(pubkey)`, then verifies the signature under it.
    /// Empty in sim/test constructors that never call `verify_signature`.
    pub pubkey: PubKeyBytes,
}

impl SignedTx {
    /// Cheap pre-validation: scheme/sig-length sanity, sender == fee_payer.
    /// Does NOT verify the actual signature (deferred to flux-eternal-cypher).
    pub fn precheck(&self) -> Result<(), TxApplyError> {
        // Shielded spends carry NEITHER a transparent payer NOR a wallet signature:
        // authorization is the STARK, which `commit_state_transition` verifies. Every
        // check below assumes a signing wallet, so they are skipped rather than fed a
        // dummy key — requiring a signature would force each shielded send to name a
        // wallet, recreating the linkage the pool exists to break. This is safe because
        // it defers to a strictly stronger check that cannot be bypassed, not because
        // the transaction is unauthenticated.
        if matches!(self.tx, SigilTx::ShieldedSend { .. } | SigilTx::Unshield { .. }) {
            return Ok(());
        }
        if self.sig.0.len() != self.sig_scheme.expected_sig_len() {
            return Err(TxApplyError::SigLengthMismatch {
                scheme: self.sig_scheme,
                expected: self.sig_scheme.expected_sig_len(),
                got: self.sig.0.len(),
            });
        }
        if self.from_pubkey != self.tx.fee_payer() {
            return Err(TxApplyError::SignerNotPayer);
        }
        Ok(())
    }

    /// Full crypto verification — the block-ingest gate. Cost lives here, NOT
    /// in `apply_tx` (which only `precheck`s): a sig is verified ONCE on
    /// ingest, then the tx is ordered + applied by hash without re-verify
    /// (the "verify-once" structural lever). `flux_ai_audit` flags any state
    /// mutation whose tx didn't pass through this chokepoint.
    ///
    /// Three checks, in order of cost (cheap → expensive):
    /// 1. pubkey length matches the declared scheme,
    /// 2. account binding `from_pubkey == BLAKE3(pubkey)`,
    /// 3. the actual signature, dispatched by `sig_scheme` (the agility seam).
    pub fn verify_signature(&self) -> Result<(), TxApplyError> {
        self.precheck()?;
        let want_pk = self.sig_scheme.expected_pubkey_len();
        if self.pubkey.0.len() != want_pk {
            return Err(TxApplyError::PubKeyLengthMismatch {
                scheme: self.sig_scheme,
                expected: want_pk,
                got: self.pubkey.0.len(),
            });
        }
        if wallet_id_from_pubkey(&self.pubkey.0) != self.from_pubkey {
            return Err(TxApplyError::WalletBindingMismatch);
        }
        let digest = self.tx.hash();
        match self.sig_scheme {
            SigScheme::SqiSign5 => match flux_sqisign::verify(&digest, &self.sig.0, &self.pubkey.0) {
                Ok(true) => Ok(()),
                Ok(false) => Err(TxApplyError::SignatureInvalid),
                Err(e) => Err(TxApplyError::SignatureMalformed(e)),
            },
            // No Dilithium5 verifier yet (flux-sqisign hybrid: "integration
            // pending"). Fail loud rather than silently accept.
            SigScheme::Dilithium5 => Err(TxApplyError::NotImplemented(
                "Dilithium5 verify pending flux-zk dilithium integration",
            )),
            // Hot-path classical scheme (crypto-agility split). Lengths already
            // checked above (32-byte pubkey, 64-byte sig).
            SigScheme::Ed25519Hot => {
                use ed25519_dalek::{Signature, Verifier, VerifyingKey};
                let pk: [u8; 32] = self.pubkey.0.as_slice().try_into()
                    .map_err(|_| TxApplyError::SignatureInvalid)?;
                let vk = VerifyingKey::from_bytes(&pk)
                    .map_err(|_| TxApplyError::SignatureInvalid)?;
                let sg: [u8; 64] = self.sig.0.as_slice().try_into()
                    .map_err(|_| TxApplyError::SignatureInvalid)?;
                let sig = Signature::from_bytes(&sg);
                vk.verify(&digest, &sig).map_err(|_| TxApplyError::SignatureInvalid)
            }
            // 2026-08-20: HybridSqiEd25519 exists for BLOCK-header producer
            // signatures (see sigil-header/sigil-node's producer_signing) — not
            // yet wired for per-tx signing. Fail loud rather than silently
            // accept an unverified tx, same posture as Dilithium5 above.
            SigScheme::HybridSqiEd25519 => Err(TxApplyError::NotImplemented(
                "HybridSqiEd25519 tx verification not yet wired (block-header scheme only)",
            )),
        }
    }
}

// ── Hot-path helpers + the verify-once Mempool ───────────────────────────────

/// Generate an ed25519 hot-path keypair: returns `(signing_key_bytes,
/// pubkey_bytes, wallet_id)`. The wallet id is the chain address
/// `BLAKE3(pubkey)` — the same binding [`SignedTx::verify_signature`] enforces.
pub fn ed25519_keygen() -> ([u8; 32], [u8; 32], WalletId) {
    use ed25519_dalek::SigningKey;
    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.verifying_key().to_bytes();
    let wallet = wallet_id_from_pubkey(&pk);
    (sk.to_bytes(), pk, wallet)
}

/// Sign `tx` with an ed25519 hot-path key, producing a fully-formed
/// [`SignedTx`] that passes [`SignedTx::verify_signature`]. Signs `tx.hash()`
/// (the canonical digest), exactly what the verifier checks.
pub fn ed25519_sign_tx(tx: SigilTx, sk_bytes: &[u8; 32], pk: &[u8; 32]) -> SignedTx {
    use ed25519_dalek::{Signer, SigningKey};
    let sk = SigningKey::from_bytes(sk_bytes);
    let sig = sk.sign(&tx.hash()).to_bytes().to_vec();
    SignedTx {
        from_pubkey: wallet_id_from_pubkey(pk),
        tx,
        nonce: 0,
        sig_scheme: SigScheme::Ed25519Hot,
        sig: SignatureBytes(sig),
        pubkey: PubKeyBytes(pk.to_vec()),
    }
}

/// Outcome of a [`Mempool::ingest`] batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MempoolIngest {
    /// Newly-verified txs added to the pool.
    pub accepted: usize,
    /// Txs rejected because the signature/binding was invalid.
    pub invalid: usize,
    /// Txs skipped because their hash was already seen (dup-spend / replay).
    pub dupe: usize,
}

/// Verify a batch of signed txs ONCE and PARTITION into (valid, invalid).
///
/// Unlike [`verify_batch_parallel`] (which stops at the first failure — right
/// for block validation where one bad tx voids the block), a mempool must keep
/// the good txs and drop only the bad. The happy path (an all-ed25519 chunk
/// with all signatures valid) takes the amortized batch-MSM fast path
/// (`ed25519_dalek::verify_batch`) per core; only a chunk that fails the batch
/// falls back to per-tx attribution. SQIsign txs always go per-tx (no batch MSM).
pub fn verify_partition_parallel(txs: Vec<SignedTx>) -> (Vec<SignedTx>, Vec<SignedTx>) {
    if txs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let chunk = txs.len().div_ceil(cores);
    // verdicts[i] = Some(true) valid / Some(false) invalid; index-aligned to txs.
    let verdicts: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(vec![false; txs.len()]);
    std::thread::scope(|s| {
        for (ci, sl) in txs.chunks(chunk).enumerate() {
            let base = ci * chunk;
            let v = &verdicts;
            s.spawn(move || {
                let local = verify_chunk(sl);
                let mut g = v.lock().unwrap();
                for (i, ok) in local.into_iter().enumerate() {
                    g[base + i] = ok;
                }
            });
        }
    });
    let verdicts = verdicts.into_inner().unwrap();
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for (tx, ok) in txs.into_iter().zip(verdicts) {
        if ok { valid.push(tx); } else { invalid.push(tx); }
    }
    (valid, invalid)
}

/// Verify one chunk, returning a per-tx ok/bad verdict. Tries the ed25519
/// batch-MSM fast path when the whole chunk is ed25519; falls back to per-tx.
fn verify_chunk(txs: &[SignedTx]) -> Vec<bool> {
    let all_ed = !txs.is_empty()
        && txs.iter().all(|t| t.sig_scheme == SigScheme::Ed25519Hot);
    if all_ed {
        if let Some(verdicts) = ed25519_batch_verify(txs) {
            return verdicts;
        }
    }
    txs.iter().map(|t| t.verify_signature().is_ok()).collect()
}

/// Batch-verify an all-ed25519 chunk via amortized MSM. Returns `Some(all-true)`
/// when every tx passes the cheap checks (length + `BLAKE3(pubkey)` binding) AND
/// the batch signature check passes; returns `None` to signal "fall back to
/// per-tx" if any structural check fails or the batch verify rejects (so the
/// caller can attribute exactly which txs are bad).
fn ed25519_batch_verify(txs: &[SignedTx]) -> Option<Vec<bool>> {
    use ed25519_dalek::{Signature, VerifyingKey};
    let mut digests: Vec<[u8; 32]> = Vec::with_capacity(txs.len());
    let mut sigs: Vec<Signature> = Vec::with_capacity(txs.len());
    let mut vks: Vec<VerifyingKey> = Vec::with_capacity(txs.len());
    for t in txs {
        if t.precheck().is_err() || t.pubkey.0.len() != 32 { return None; }
        if wallet_id_from_pubkey(&t.pubkey.0) != t.from_pubkey { return None; }
        let pk: [u8; 32] = t.pubkey.0.as_slice().try_into().ok()?;
        let vk = VerifyingKey::from_bytes(&pk).ok()?;
        let sg: [u8; 64] = t.sig.0.as_slice().try_into().ok()?;
        digests.push(t.tx.hash());
        sigs.push(Signature::from_bytes(&sg));
        vks.push(vk);
    }
    let msgs: Vec<&[u8]> = digests.iter().map(|d| d.as_slice()).collect();
    match ed25519_dalek::verify_batch(&msgs, &sigs, &vks) {
        Ok(()) => Some(vec![true; txs.len()]),
        Err(_) => None, // mixed validity — let the per-tx path attribute it
    }
}

/// The verify-once mempool: signatures are checked exactly ONCE here, on
/// ingest. [`Mempool::pull`] hands verified txs to the block producer WITHOUT
/// re-verification — the structural lever (Narwhal) that decouples the
/// signature wall from block production. Dedups by tx hash (replay / dup-spend).
#[derive(Default)]
pub struct Mempool {
    verified: std::collections::VecDeque<SignedTx>,
    seen: std::collections::HashSet<[u8; 32]>,
    verified_total: u64,
    /// R1: the BATCH LANE — whole AuthorizedBatches awaiting inclusion, kept INTACT
    /// so verifiers amortize one signature per batch. NEVER exploded into `verified`
    /// (batch ops are bare SigilTx with no per-tx envelope).
    batches: std::collections::VecDeque<AuthorizedBatch>,
    /// Dedup keys (the signed auth message) for batches seen this mempool lifetime.
    seen_batches: std::collections::HashSet<[u8; 32]>,
    batch_ops_total: u64,
}

impl Mempool {
    pub fn new() -> Self { Self::default() }

    /// Dedup, then verify the fresh txs ONCE and store the valid ones.
    pub fn ingest(&mut self, txs: Vec<SignedTx>) -> MempoolIngest {
        let mut fresh = Vec::with_capacity(txs.len());
        let mut dupe = 0usize;
        for t in txs {
            // dedup key = the intent hash (replay / dup-spend protection); two
            // signed envelopes of the same intent+nonce are the same tx.
            if self.seen.contains(&t.tx.hash()) { dupe += 1; } else { fresh.push(t); }
        }
        let (valid, invalid) = verify_partition_parallel(fresh);
        for t in &valid { self.seen.insert(t.tx.hash()); }
        self.verified_total += valid.len() as u64;
        let out = MempoolIngest { accepted: valid.len(), invalid: invalid.len(), dupe };
        self.verified.extend(valid);
        out
    }

    /// Pull up to `max` verified txs for block inclusion. NO re-verification —
    /// these were verified once on ingest.
    pub fn pull(&mut self, max: usize) -> Vec<SignedTx> {
        let n = max.min(self.verified.len());
        self.verified.drain(..n).collect()
    }

    /// Verified txs currently awaiting inclusion.
    pub fn len(&self) -> usize { self.verified.len() }
    pub fn is_empty(&self) -> bool { self.verified.is_empty() }
    /// Total signatures verified over this mempool's life (verify-once meter).
    pub fn verified_total(&self) -> u64 { self.verified_total }

    /// True if this tx hash has been seen (ingested) by this mempool — used by
    /// the money API's `/v1/transactions/:hash` status route.
    pub fn contains(&self, hash: &[u8; 32]) -> bool { self.seen.contains(hash) }

    /// R1: ingest an AuthorizedBatch. Verifies the ONE signature (sig + single-author
    /// + R0 nonce binding) ONCE, dedups by the signed auth message, and enqueues the
    /// batch INTACT. Returns the op count accepted. The consensus replay nonce is
    /// enforced later, at apply, by `SigilState::check_and_bump_nonce`.
    pub fn ingest_batch(&mut self, batch: AuthorizedBatch) -> Result<usize, TxApplyError> {
        batch.verify()?;
        let key = batch_auth_message(&batch.author, batch.nonce, &batch.ops);
        if !self.seen_batches.insert(key) {
            return Err(TxApplyError::DuplicateBatch);
        }
        let ops = batch.ops.len();
        self.batch_ops_total += ops as u64;
        self.batches.push_back(batch);
        Ok(ops)
    }

    /// Pull whole batches for block inclusion, up to ~`max_ops` operations. Never
    /// splits a batch (the signature covers the whole op set); always takes at least
    /// one batch if any are pending.
    pub fn pull_batches(&mut self, max_ops: usize) -> Vec<AuthorizedBatch> {
        // Take whole batches until the op budget is MET (overshoot by at most one
        // batch — a batch is never split, its signature covers the whole op set).
        let mut out = Vec::new();
        let mut ops = 0usize;
        while let Some(front) = self.batches.front() {
            ops += front.ops.len();
            out.push(self.batches.pop_front().unwrap());
            if ops >= max_ops { break; }
        }
        out
    }

    /// Batches awaiting inclusion.
    pub fn batch_count(&self) -> usize { self.batches.len() }
    /// Total ops across pending batches (the demand signal for the rate governor).
    pub fn pending_batch_ops(&self) -> usize { self.batches.iter().map(|b| b.ops.len()).sum() }
}

// ── AuthorizedBatch — one signature authorizes N operations ──────────────────

/// BLAKE3 commitment over a batch's operation hashes — the message the author
/// signs. Re-deriving it at verify time binds the signature to EXACTLY these
/// ops in this order; a forged, added, or reordered op changes the root.
pub fn batch_root(ops: &[SigilTx]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&(ops.len() as u64).to_le_bytes());
    for op in ops { h.update(&op.hash()); }
    *h.finalize().as_bytes()
}

/// R0: the message an author signs to authorize a batch. Binds the author, a
/// monotonic replay `nonce`, AND the exact op set (via [`batch_root`]). Changing any
/// of the three invalidates the signature — closing the batch-replay hole a bare
/// `batch_root` signature left open (a public batch could be rebroadcast + re-executed
/// once the mempool's ephemeral seen-set cleared on restart).
pub fn batch_auth_message(author: &WalletId, nonce: u64, ops: &[SigilTx]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"SIGIL/batch-auth/v1");
    h.update(author);
    h.update(&nonce.to_le_bytes());
    h.update(&batch_root(ops));
    *h.finalize().as_bytes()
}

/// A batch of operations authorized by ONE signature over their commitment.
///
/// The structural lever that turns the "free" state-commit ceiling into usable
/// TPS: verify ONE signature, then apply N ops at state-fold speed. The
/// signature amortizes away as the batch grows (per-op cost → hash + fold,
/// not sig + fold).
///
/// SOUND for a SINGLE author: every op's `fee_payer()` must equal `author`, so
/// the batch can only move the author's own funds. The signature over
/// [`batch_root`] binds the author to this exact op set. Cross-author batching
/// would need aggregate signatures (out of scope — that's the BLS/PQ-aggregate
/// lane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedBatch {
    /// The single account authorizing every op in `ops`.
    pub author: WalletId,
    /// R0: per-author monotonic replay nonce, BOUND into the signature via
    /// [`batch_auth_message`]. Enforced at apply by `SigilState::check_and_bump_nonce`;
    /// a rebroadcast batch with a changed nonce fails `verify()`, and the same
    /// `(author, nonce)` is rejected as a replay by the state.
    pub nonce: u64,
    /// Full scheme public key (bound: `author == BLAKE3(pubkey)`).
    pub pubkey: PubKeyBytes,
    /// Signature scheme.
    pub sig_scheme: SigScheme,
    /// ONE signature over `batch_root(ops)`.
    pub sig: SignatureBytes,
    /// The authorized operations (all authored by `author`).
    pub ops: Vec<SigilTx>,
}

impl AuthorizedBatch {
    /// Build an ed25519 hot-path authorized batch. All `ops` MUST be the
    /// author's own (caller's responsibility; [`Self::verify`] enforces it).
    pub fn sign_ed25519(ops: Vec<SigilTx>, nonce: u64, sk: &[u8; 32], pk: &[u8; 32]) -> Self {
        use ed25519_dalek::{Signer, SigningKey};
        let author = wallet_id_from_pubkey(pk);
        let msg = batch_auth_message(&author, nonce, &ops);
        let sig = SigningKey::from_bytes(sk).sign(&msg).to_bytes().to_vec();
        Self {
            author,
            pubkey: PubKeyBytes(pk.to_vec()),
            sig_scheme: SigScheme::Ed25519Hot,
            sig: SignatureBytes(sig),
            nonce,
            ops,
        }
    }

    /// Verify the batch: ONE signature over the commitment + every op is the
    /// author's. Cost is O(1) signatures regardless of `ops.len()` — the lever.
    pub fn verify(&self) -> Result<(), TxApplyError> {
        // 1. single-author soundness: no op may move someone else's funds.
        for op in &self.ops {
            if op.fee_payer() != self.author {
                return Err(TxApplyError::SignerNotPayer);
            }
        }
        // 2. pubkey binds to the author account.
        if self.pubkey.0.len() != self.sig_scheme.expected_pubkey_len()
            || wallet_id_from_pubkey(&self.pubkey.0) != self.author
        {
            return Err(TxApplyError::WalletBindingMismatch);
        }
        // 3. ONE signature over the re-derived auth message (binds author + nonce +
        //    this exact op set — closes the replay hole).
        let root = batch_auth_message(&self.author, self.nonce, &self.ops);
        match self.sig_scheme {
            SigScheme::Ed25519Hot => {
                use ed25519_dalek::{Signature, Verifier, VerifyingKey};
                let pk: [u8; 32] = self.pubkey.0.as_slice().try_into()
                    .map_err(|_| TxApplyError::SignatureInvalid)?;
                let vk = VerifyingKey::from_bytes(&pk).map_err(|_| TxApplyError::SignatureInvalid)?;
                let sg: [u8; 64] = self.sig.0.as_slice().try_into()
                    .map_err(|_| TxApplyError::SignatureInvalid)?;
                vk.verify(&root, &Signature::from_bytes(&sg)).map_err(|_| TxApplyError::SignatureInvalid)
            }
            SigScheme::SqiSign5 => match flux_sqisign::verify(&root, &self.sig.0, &self.pubkey.0) {
                Ok(true) => Ok(()),
                Ok(false) => Err(TxApplyError::SignatureInvalid),
                Err(e) => Err(TxApplyError::SignatureMalformed(e)),
            },
            SigScheme::Dilithium5 => Err(TxApplyError::NotImplemented(
                "Dilithium5 batch verify pending flux-zk dilithium integration",
            )),
            SigScheme::HybridSqiEd25519 => Err(TxApplyError::NotImplemented(
                "HybridSqiEd25519 batch verify not yet wired (block-header scheme only)",
            )),
        }
    }

    /// Number of operations this single signature authorizes.
    pub fn len(&self) -> usize { self.ops.len() }
    pub fn is_empty(&self) -> bool { self.ops.is_empty() }
}

/// The account id a public key binds to: the chain's address IS `BLAKE3(pubkey)`.
/// Enforced by [`SignedTx::verify_signature`] so a valid signature under a
/// *different* key than the claimed `from_pubkey` account is rejected.
pub fn wallet_id_from_pubkey(pubkey: &[u8]) -> WalletId {
    *blake3::hash(pubkey).as_bytes()
}

/// Verify a batch of signed txs across ALL cores — the Tier-1 sig-wall lift.
///
/// Post-quantum signatures (SQIsign / Dilithium) have no batch-MSM shortcut
/// the way ed25519 does, so the win here is raw parallelism: N verifies fan
/// out over the machine's cores via `std::thread::scope`. Call this ONCE at
/// block ingest; `apply_tx` never re-verifies.
///
/// Returns `Err((index, error))` for the LOWEST-index tx that fails
/// (deterministic regardless of thread scheduling), else `Ok(())`.
pub fn verify_batch_parallel(txs: &[SignedTx]) -> Result<(), (usize, TxApplyError)> {
    if txs.is_empty() {
        return Ok(());
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let chunk = txs.len().div_ceil(cores);
    let first_err: std::sync::Mutex<Option<(usize, TxApplyError)>> = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        for (ci, sl) in txs.chunks(chunk).enumerate() {
            let base = ci * chunk;
            let fe = &first_err;
            s.spawn(move || {
                for (i, tx) in sl.iter().enumerate() {
                    if let Err(e) = tx.verify_signature() {
                        let idx = base + i;
                        let mut g = fe.lock().unwrap();
                        // keep the lowest failing index for determinism
                        match &*g {
                            Some((j, _)) if *j <= idx => {}
                            _ => *g = Some((idx, e)),
                        }
                        break; // this chunk already has a (lower-or-equal) failure
                    }
                }
            });
        }
    });
    match first_err.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ── Apply ───────────────────────────────────────────────────────────────────

/// Errors at the tx layer. Strictly distinct from `sigil_state`'s
/// `CommitError` and `sigil_header`'s `HeaderError` — the chain owns
/// each layer's failure modes separately so the node can route the right
/// HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum TxApplyError {
    /// Insufficient balance to cover (amount + fee) on a Send/Swap/etc.
    #[error("insufficient balance: wallet has {have}, needs {need}")]
    InsufficientBalance {
        /// What the wallet had at apply time.
        have: u128,
        /// What the tx needed.
        need: u128,
    },

    /// The signer wasn't the fee payer.
    #[error("signer != fee_payer: txs MUST be signed by their fee payer")]
    SignerNotPayer,

    /// R1: this AuthorizedBatch is already in the mempool (dedup by signed auth message).
    #[error("duplicate batch: already in the mempool")]
    DuplicateBatch,

    /// Signature bytes were the wrong length for the declared scheme.
    #[error("sig length mismatch: scheme {scheme:?} expected {expected}, got {got}")]
    SigLengthMismatch {
        /// Scheme declared.
        scheme: SigScheme,
        /// Bytes expected.
        expected: usize,
        /// Bytes actually present.
        got: usize,
    },

    /// Swap output dipped below the user's `min_out` slippage guard.
    #[error("slippage exceeded: min_out {min_out}, got {actual}")]
    SlippageExceeded {
        /// User's declared minimum acceptable output.
        min_out: u128,
        /// What the pool actually delivered.
        actual: u128,
    },

    /// Pool referenced in a Swap/LpDeposit/LpWithdraw doesn't exist.
    #[error("pool not found")]
    PoolNotFound,

    /// Math hit a multiply / add overflow. SIGIL aborts the tx loudly rather
    /// than silently wrapping or saturating. The Quillon foot-gun fix.
    #[error("integer overflow in tx arithmetic")]
    Overflow,

    /// The tx's declared `token_a`/`token_b`/`fee_bps` don't match the
    /// pool's recorded shape. On first deposit the tx defines them; on
    /// subsequent ops the tx must mirror them exactly.
    #[error("pool mismatch: tx pair/fee disagrees with pool state")]
    PoolMismatch,

    /// Swap's `in_token` is neither `pool.token_a` nor `pool.token_b`.
    #[error("invalid swap token: in_token not in pool")]
    InvalidSwapToken,

    /// `LpWithdraw` tried to burn more LP shares than the caller owns.
    ///
    /// Before the per-wallet LP ledger existed, `LpWithdraw` checked only that
    /// the caller could pay the flat fee and then burned shares straight out of
    /// the POOL TOTAL — so any wallet could withdraw a pool it never deposited
    /// into. Proven drainable end-to-end with real ed25519 keys in commit
    /// 86e7094 (`MALLORY` deposits nothing, withdraws the entire reserve).
    /// This is the error that PoC now hits.
    #[error("insufficient LP shares: wallet owns {have} in this pool, tried to burn {need}")]
    InsufficientLpShares {
        /// LP shares the wallet actually owns in this pool.
        have: u128,
        /// LP shares the tx tried to burn.
        need: u128,
    },

    /// A pool's `token_a`/`token_b` collides with that pool's own derived LP
    /// share token id. Astronomically improbable by accident (it needs a BLAKE3
    /// preimage), so it is treated as hostile: allowing it would alias the LP
    /// ledger slot onto a reserve-token slot and let one `SetBalance` silently
    /// overwrite the other — the same multi-mutation aliasing class the
    /// deposit/withdraw handlers already defend against for NATIVE.
    #[error("pool token collides with the pool's own LP share token id")]
    LpTokenCollision,

    /// AMM math (sigil-dex) raised a guard. Carries the underlying variant.
    #[error("dex error: {0}")]
    Dex(#[from] sigil_dex::DexError),

    /// USDS mint/redeem math (sigil-usds) raised a guard. Carries the
    /// underlying variant.
    #[error("usds error: {0}")]
    Usds(#[from] sigil_usds::UsdsError),

    /// Public-key bytes were the wrong length for the declared scheme.
    #[error("pubkey length mismatch: scheme {scheme:?} expected {expected}, got {got}")]
    PubKeyLengthMismatch {
        /// Scheme declared.
        scheme: SigScheme,
        /// Pubkey bytes expected.
        expected: usize,
        /// Pubkey bytes actually present.
        got: usize,
    },

    /// `from_pubkey` account != `BLAKE3(pubkey)` — the carried key doesn't own
    /// the claimed account. Blocks presenting a valid sig under a foreign key.
    #[error("wallet binding mismatch: from_pubkey != BLAKE3(pubkey)")]
    WalletBindingMismatch,

    /// Signature was well-formed but did not verify against (pubkey, tx.hash()).
    #[error("signature invalid: did not verify under the carried pubkey")]
    SignatureInvalid,

    /// Signature/pubkey bytes were structurally rejected by the verifier.
    #[error("signature malformed: {0}")]
    SignatureMalformed(String),

    /// Feature isn't wired yet — kept loud so the caller doesn't quietly
    /// commit something half-baked.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),

    /// A transparent peer-to-peer send was submitted at or after
    /// [`SHIELDED_ONLY_HEIGHT`]. SIGIL is privacy-only from that height: shield the value
    /// and use a shielded send. Shield/Unshield remain available as the ramps.
    #[error(
        "transparent sends are retired as of height {activated_at} (this tx at {height}): \
         SIGIL is privacy-only — use Shield then ShieldedSend, or Unshield to exit"
    )]
    TransparentSendRetired { height: u64, activated_at: u64 },

    /// A shielded transaction was refused before reaching the chokepoint. This layer only
    /// catches the cheap, obvious cases (a nullifier already in the spent set); the
    /// authoritative checks — anchor validity and the STARK — belong to
    /// `commit_state_transition` and are never skipped because of a pass here.
    #[error("shielded tx rejected: {0}")]
    ShieldedRejected(&'static str),

    /// A SIGIL-Nation transaction (CitizenAttest / WelfareClaim) arrived
    /// before the feature's activation height.
    #[error("nation feature not active: activates at height {activates_at}, tx at {height}")]
    NationNotActive { height: u64, activates_at: u64 },

    /// CitizenAttest signed by a wallet that is not the chain's master
    /// wallet (or the chain has no master wallet committed).
    #[error("attest refused: signer is not the nation authority (master wallet)")]
    NotNationAuthority,
    /// OraclePush with a zero price — zero is how "no oracle" is
    /// represented, so pushing it would be an un-push, silently re-bricking
    /// every welfare claim.
    #[error("oracle price must be non-zero")]
    ZeroOraclePrice,

    /// CitizenAttest with an all-zero cpr_hash — an empty attestation
    /// would be indistinguishable from "not a citizen".
    #[error("attest refused: cpr_hash must be non-zero")]
    InvalidAttestation,

    /// WelfareClaim from a wallet with no borger-registry attestation.
    #[error("welfare refused: wallet is not an attested citizen")]
    NotCitizen,

    /// WelfareClaim inside the per-citizen cooldown window.
    #[error("welfare cooldown: next claim allowed at height {next_height}")]
    WelfareCooldown { next_height: u64 },

    /// The welfare treasury cannot cover the stipend. Welfare never mints —
    /// an underfunded treasury refuses instead.
    #[error("welfare treasury underfunded: has {have}, stipend is {need}")]
    WelfareTreasuryInsufficient { have: u128, need: u128 },
}

/// Result of applying one tx: the atomic batch of state mutations + the
/// events that should be appended to the block's event log. Caller folds
/// these into the block's `StateTransition` and event vec.
#[derive(Debug, Clone, Default)]
pub struct ApplyResult {
    /// Mutations to feed to `commit_state_transition`.
    pub mutations: Vec<StateMutation>,
    /// Events to record in the block's event log.
    pub events: Vec<SigilEvent>,
}

/// Apply one signed tx against an immutable read view of state. The caller
/// is responsible for applying the returned mutations atomically and
/// pushing the resulting event leaf-hashes into the same StateTransition
/// (so the `event_log_root` reflects the events).
///
/// Phase 0 behavior:
/// - Skips signature verification (delegated to caller in P1).
/// - Only the wallet-affecting kinds (`Send`, `MintReward` via `apply_tx`)
///   produce real mutations; DEX/VM/validator kinds emit events but no
///   storage changes yet — their wiring lands when those crates port.
/// Height at which SIGIL becomes PRIVACY-ONLY: transparent peer-to-peer
/// [`SigilTx::Send`] stops being accepted, and value transfer between parties must go
/// through the shielded pool.
///
/// # Why a height and not a deletion
///
/// Deleting the variant would change how ALREADY-SETTLED blocks validate — every historical
/// transparent send would become invalid and the chain would fail to replay from genesis.
/// The mainnet rule is that old blocks must always validate the same way, so the transition
/// is a height gate: below it the old rule, at or above it the new one. A node can then
/// replay the entire chain with one binary.
///
/// # What stays transparent, and why that is not a loophole
///
/// [`SigilTx::Shield`] and [`SigilTx::Unshield`] still touch transparent balances, because
/// they are the on- and off-ramps — a pool nobody can enter or leave is not privacy, it is
/// a trap. Mining rewards and DEX settlement also land transparently. What ends here is
/// *paying another party in the clear*: to move value to someone else you shield, send
/// privately, and they hold or unshield. The amounts and the link between payer and payee
/// are what the pool hides, and those are exactly what a transparent `Send` published.
///
/// Set to `0` deliberately: nothing is deployed, every shielded pool is empty, and there is
/// no settled history containing a transparent send that matters. Raise this to a future
/// height before any deployment that already carries real transparent traffic.
pub const SHIELDED_ONLY_HEIGHT: u64 = 0;

/// Height from which a shielded spend may declare MORE THAN ONE input.
///
/// Below it, `extra_nullifiers` must be empty and a transaction carrying any is refused —
/// so the multi-input circuit cannot be reached by a transaction until every node agrees it
/// is allowed to be, and blocks already on the chain validate exactly as they did.
///
/// Sequenced AFTER [`TRANSPARENT_COINBASE_HEIGHT`] (500,000) on purpose. The two changes are
/// independent, and putting them at one height would mean a problem with either forces
/// reverting both. This one is also PERMISSIVE — it only allows something new — so it can
/// activate late at no cost, and nothing can even produce such a transaction until wallets
/// support it.
///
/// ZERO ON `sigil-g2`. Same reasoning as [`TRANSPARENT_COINBASE_HEIGHT`]: a fresh chain has
/// no history to invalidate, so sequencing the two changes apart buys nothing. It also means
/// a two-note merge is testable the day the chain starts rather than ~2 days later, which is
/// the whole reason the reset was worth doing rather than waiting out a window.
///
/// (Previously 600,000, deliberately after the coinbase change so a problem with either
/// would not force reverting both. That sequencing matters on a LIVE chain and is the shape
/// to copy for any future activation.)
pub const SHIELDED_MULTI_INPUT_HEIGHT: u64 = 0;

/// May a spend at `height` declare more than one input?
pub fn multi_input_spend_allowed(height: u64) -> bool {
    height >= SHIELDED_MULTI_INPUT_HEIGHT
}

/// Height from which a wallet that has published a SQIsign L5 key MUST also sign its
/// shielded ramps (`Shield`, `Unshield`) with that key — require-both, never either-or.
///
/// DORMANT (`u64::MAX`) until an operator schedules a real height. That is deliberate and
/// it is the whole point of this constant:
///
///   * ENFORCEMENT IS A CONSENSUS RULE. If some nodes require the SQIsign leg and others
///     do not, they disagree about whether a transaction is VALID, and the chain splits.
///     A height gate is what lets the whole network change its mind at the same block
///     instead of whenever each operator happened to restart.
///   * IT CAN LOCK PEOPLE OUT. A registered key has no removal path (see
///     `shielded::ShieldedPool::sqi_keys` for why removal would hand the attack back to
///     the attacker). Anyone who registers a key and loses it can no longer ramp, ever.
///     Activation must not surprise such a user.
///
/// Below this height a registered SQIsign key is recorded and PROVEN (the registration
/// carries a possession signature) but never demanded — so a wallet can upgrade, verify
/// the whole path works end to end, and be ready long before the rule bites. Set the
/// height far enough ahead that every holder has had time to do exactly that.
pub const SQI_RAMP_REQUIRED_HEIGHT: u64 = u64::MAX;

/// Height from which a coinbase pays its cuts TRANSPARENTLY, even to a wallet that has
/// registered a shielded address.
///
/// WHAT THIS FIXES. A shielded coinbase mints one note per payee per block. At ~6.28
/// blocks/second that is ~543,000 notes per payee per day, each one worth a single block
/// reward. The spend circuit is 1-in/2-out: one note in, one payment and one change note
/// out. So a miner's balance is real, and almost all of it is unspendable — not locked by
/// a bug, but by arithmetic. To send you must pick ONE note, and one note is one block's
/// reward. Measured live at height 311,862: epoch 25, 822,556 notes, 34 registered
/// wallets, and exactly ONE nullifier ever revealed. A write-only pool.
///
/// The pathology had an ugly shape: a miner who never registered was paid with
/// `SetBalance`, which ADDS, and could spend everything. A miner who registered — who
/// opted into privacy — got dust. Registering for privacy is what broke spending.
///
/// WHY TRANSPARENT IS ALSO THE MORE HONEST CHOICE, not just the simpler one. It is
/// tempting to read this as trading privacy for usability. It is not, because the privacy
/// being given up here was already nil. A coinbase note is minted in the block its owner
/// just mined, and that block names the miner. `sigil-chronos` measured the linkage
/// directly: 620 of 620 coinbase notes were publicly attributable to their miner at mint
/// time. They enlarged the note count without enlarging the anonymity set, which is
/// measured in distinct unlinkable OWNERS, not in notes. 822,556 attributable dust notes
/// are padding, not cover.
///
/// A deliberate `Shield` is strictly better cover than a coinbase note on every axis that
/// matters. Its timing is chosen by the user rather than forced to the block they mined;
/// its amount is a standard denomination rather than an exact block reward; and it mints
/// ONE note big enough to actually spend. The transparent balance becomes the accrual
/// bucket, which is why this needs no new consensus state: `SetBalance` already adds.
///
/// So the flow after this height is: mine → balance grows → `Shield` once, for as much as
/// you like → spend it privately. Rather than: mine → 543,000 unspendable crumbs a day.
///
/// GATED BY HEIGHT, NOT BY ENV. A coinbase is part of the block body, so two nodes
/// disagreeing about how to pay it produce different blocks and the chain forks. An
/// environment variable would make that disagreement a deployment accident; a height makes
/// every node switch at the same block regardless of when its operator restarts. Below
/// this height the old shielded-coinbase path is preserved EXACTLY, so all 311k existing
/// blocks still validate byte-for-byte — the golden rule for a live chain.
///
/// ZERO ON `sigil-g2` (2026-08-28). A rollout window exists to let a running chain and its
/// followers cross a rule change together. A chain that starts at block 0 under the new rule
/// has nothing to cross: there is no history built under the old rule, and every node needs
/// the new binary to join at all. So the window is not shortened here, it is *unnecessary* —
/// and the dust the old rule produced (836,536 notes, 230,000/day) never begins.
///
/// ⚠️ NOT lowerable in place on a chain that already has history. Setting this to 0 on the
/// g1 chain would have declared all ~317,000 existing blocks invalid, because they were
/// built under the shielded-coinbase rule. Gates to zero and a chain reset are ONE
/// indivisible change.
///
/// The window arithmetic below is kept because it is the reasoning any FUTURE activation on
/// a live chain will need, and because both measurements in it were hard-won.
///
/// SIZING THE ACTIVATION WINDOW (for a live chain — not needed here). Two things were
/// measured before picking the old number, and both moved it:
///
///   * The block rate is **2.66 blk/s**, sampled over 90 s at height 317,141 — not the
///     6.28 blk/s carried in older notes, which was a catch-up rate, not the steady one.
///     Deriving the window from the wrong rate makes it 2.4x shorter than it looks.
///   * The node has **7 peers, 3 of them on genuinely external IPs**. A coinbase is part
///     of the block body, so a follower still running the old rule recomputes a different
///     `wallet_state_root` at this height and falls off the chain. They update through the
///     signed `sigil-top-latest.json` manifest, which clients poll on their own schedule —
///     so the window must cover a poll cycle plus the release itself, not just a restart.
///
/// Set to 500,000 on 2026-08-28: 182,514 blocks ahead of the tip at the time, ~19 hours at
/// the measured rate. Long enough for a release to propagate to independent operators;
/// short enough that it costs only ~3,900 SIGIL of further dust (at the measured 0.0216
/// SIGIL average note) rather than the ~8,300 a 40-hour window would.
///
/// An earlier draft said 400,000 on a 6.28 blk/s assumption and before the external peers
/// were counted. Corrected here rather than amended, because the branch was already pushed.
pub const TRANSPARENT_COINBASE_HEIGHT: u64 = 0;

/// Does the coinbase at `height` pay transparently to everyone?
///
/// The single place this rule is asked, so a caller can never accidentally reimplement the
/// comparison with the boundary flipped.
pub fn coinbase_is_transparent(height: u64) -> bool {
    height >= TRANSPARENT_COINBASE_HEIGHT
}

/// Does the chain require this wallet's ramps to carry a SQIsign signature at `height`?
///
/// Two conditions, both necessary: the network has activated the rule, AND this
/// particular wallet has published a key. A wallet that never upgraded is never affected
/// — the rule hardens those who opted in, it does not conscript everyone else.
pub fn sqi_ramp_required(height: u64, wallet_has_sqi_key: bool) -> bool {
    height >= SQI_RAMP_REQUIRED_HEIGHT && wallet_has_sqi_key
}

/// Legacy entry point — applies with the gate DISABLED.
///
/// Retained because ~40 call sites (tests, the retired rpcd, chronos harnesses) predate the
/// gate and are not consensus paths. Consensus goes through [`apply_tx_at`], which the
/// block builder calls with the real height. If you are writing new code that applies a
/// transaction into a block, use [`apply_tx_at`].
pub fn apply_tx(state: &SigilState, signed: &SignedTx) -> Result<ApplyResult, TxApplyError> {
    apply_tx_inner(state, signed, None)
}

/// Apply a transaction as of `height`, enforcing height-gated consensus rules.
///
/// This is the consensus entry point: the block builder calls it with the height being
/// built, so a transparent send is refused from [`SHIELDED_ONLY_HEIGHT`] onward while
/// replay of older blocks — which must validate exactly as they did when settled — goes
/// through [`apply_tx`] and is unaffected.
pub fn apply_tx_at(
    state: &SigilState,
    signed: &SignedTx,
    height: u64,
) -> Result<ApplyResult, TxApplyError> {
    apply_tx_inner(state, signed, Some(height))
}

/// `at_height = None` means "no height context" and applies NO height-gated rule. That is
/// the honest encoding of a caller that genuinely has no height (a test, a shape probe,
/// a mempool dry-run) — passing a fake height would silently apply or skip a consensus
/// rule based on a number nobody chose.
fn apply_tx_inner(
    state: &SigilState,
    signed: &SignedTx,
    at_height: Option<u64>,
) -> Result<ApplyResult, TxApplyError> {
    signed.precheck()?;
    // PRIVACY-ONLY GATE. Checked before anything else so a rejected transparent send
    // cannot have partially mutated anything.
    if let Some(height) = at_height {
        if height >= SHIELDED_ONLY_HEIGHT {
            if let SigilTx::Send { .. } = &signed.tx {
                return Err(TxApplyError::TransparentSendRetired {
                    height,
                    activated_at: SHIELDED_ONLY_HEIGHT,
                });
            }
        }
    }
    let mut out = ApplyResult::default();

    match &signed.tx {
        // ── PV-1 shielded transactions ──────────────────────────────────────────────
        // These translate 1:1 into the shielded StateMutations. Deliberately thin: every
        // shielded rule that matters (anchor validity, nullifier freshness, and the STARK
        // itself) is enforced by `commit_state_transition`, not here. Duplicating those
        // checks at this layer would create a second place they could drift or be skipped
        // — and this layer's checks are not the ones that gate the money.
        SigilTx::RegisterShieldedAddress { wallet, pk_shield, pk_encrypt, pk_sqi, fee } => {
            let have = state.balance_of(wallet, &NATIVE);
            if have < *fee {
                return Err(TxApplyError::InsufficientBalance { have, need: *fee });
            }
            // Re-registration is allowed on purpose: a user who loses a seed must be able
            // to redirect future income without abandoning the wallet. Only rewards from
            // this point forward are affected — notes already minted stay bound to the old
            // key, which is a property of the notes, not a policy choice.
            out.mutations.push(StateMutation::RegisterShieldedAddress {
                wallet: *wallet,
                pk_shield: *pk_shield,
                pk_encrypt: *pk_encrypt,
                pk_sqi: pk_sqi.clone(),
            });
        }

        SigilTx::Shield { from, amount, cm, fee } => {
            let have = state.balance_of(from, &NATIVE);
            let need = amount.checked_add(*fee).ok_or(TxApplyError::Overflow)?;
            if have < need {
                return Err(TxApplyError::InsufficientBalance { have, need });
            }
            out.mutations.push(StateMutation::Shield {
                from: *from,
                amount: *amount,
                cm: *cm,
            });
        }

        SigilTx::ShieldedSend { anchor, nullifier, extra_nullifiers, cm_outs, fee, proof, note_ciphertexts } => {
            // MULTI-INPUT GATE. Below the activation height a spend may declare exactly one
            // input, so a transaction carrying extras is refused outright rather than
            // silently having them ignored — ignoring them would verify a 2-input proof
            // while recording one nullifier, which is the double-spend this whole gate
            // exists to make unreachable.
            // `at_height == None` is the legacy entry point, documented as "gate DISABLED"
            // — tests, harnesses and other non-consensus callers. Matching that convention
            // rather than inventing a stricter one here: consensus always arrives through
            // `apply_tx_at` with a real height, so the permissive branch is unreachable
            // from a block.
            if !extra_nullifiers.is_empty()
                && at_height.is_some_and(|h| !multi_input_spend_allowed(h))
            {
                return Err(TxApplyError::ShieldedRejected(
                    "multi-input shielded spend is not active at this height",
                ));
            }
            let nfs = {
                let mut v = Vec::with_capacity(1 + extra_nullifiers.len());
                v.push(*nullifier);
                v.extend_from_slice(extra_nullifiers);
                v
            };
            // DISTINCTNESS. Not a formality: the circuit ACCEPTS one note fed in as both
            // inputs — two independently-valid blocks, a conservation lane summing twice the
            // value, every constraint satisfied — and the chain stores nullifiers in a set,
            // so the duplicate insert is a no-op. One note burned, double the value out.
            for i in 0..nfs.len() {
                for j in (i + 1)..nfs.len() {
                    if nfs[i] == nfs[j] {
                        return Err(TxApplyError::ShieldedRejected(
                            "two inputs name the same nullifier",
                        ));
                    }
                }
            }
            // Reject an already-spent nullifier early so an obvious replay does not cost
            // a STARK verification. EVERY nullifier, not just the first. The chokepoint
            // re-checks — this is an optimization, never the guarantee.
            if nfs.iter().any(|nf| state.shielded().is_spent(nf)) {
                return Err(TxApplyError::ShieldedRejected("nullifier already spent"));
            }
            // Delivery is optional per output, but partial is not: a length that neither
            // matches `cm_outs` nor is empty means either the sender or the wire mangled
            // the positional alignment, and silently zipping a short/long vector would
            // deliver a ciphertext to the wrong output.
            if !note_ciphertexts.is_empty() && note_ciphertexts.len() != cm_outs.len() {
                return Err(TxApplyError::ShieldedRejected(
                    "note_ciphertexts length must be 0 or match cm_outs",
                ));
            }
            out.mutations.push(StateMutation::ShieldedSpend {
                anchor: *anchor,
                nullifier: *nullifier,
                extra_nullifiers: extra_nullifiers.clone(),
                cm_outs: cm_outs.clone(),
                fee: *fee,
                proof: proof.clone(),
                note_ciphertexts: note_ciphertexts.clone(),
            });
        }

        SigilTx::Unshield { to, amount, anchor, nullifier, cm_outs, proof, fee: _ } => {
            if state.shielded().is_spent(nullifier) {
                return Err(TxApplyError::ShieldedRejected("nullifier already spent"));
            }
            out.mutations.push(StateMutation::Unshield {
                to: *to,
                amount: *amount,
                anchor: *anchor,
                nullifier: *nullifier,
                cm_outs: cm_outs.clone(),
                proof: proof.clone(),
            });
        }

        SigilTx::Send { from, to, amount, token, fee } => {
            let from_native = state.balance_of(from, &NATIVE);
            let from_token  = state.balance_of(from, token);

            // Fee always paid in native SIGIL.
            if from_native < *fee {
                return Err(TxApplyError::InsufficientBalance {
                    have: from_native, need: *fee,
                });
            }
            // If the transfer token IS native, the sender must have
            // amount + fee in native.
            let need_native = if token == &NATIVE { fee.checked_add(*amount).ok_or(TxApplyError::Overflow)? } else { *fee };
            if from_native < need_native {
                return Err(TxApplyError::InsufficientBalance {
                    have: from_native, need: need_native,
                });
            }
            if token != &NATIVE && from_token < *amount {
                return Err(TxApplyError::InsufficientBalance {
                    have: from_token, need: *amount,
                });
            }

            // Compute per-slot final balances FIRST, then emit one SetBalance
            // per UNIQUE (wallet, token) slot. This is aliasing-safe: a
            // self-transfer (from == to) must NOT mint. With naive
            // debit-then-credit, the recipient credit (which reads pre-state)
            // overwrites the sender debit on the same slot via last-writer-win,
            // leaving balance = from + amount — minting `amount` for free. The
            // LpDeposit/LpWithdraw handlers already guard this same class; Send
            // must too.
            let self_send = from == to;
            if token == &NATIVE {
                if self_send {
                    // Single (from, NATIVE) slot: amount cancels, net is −fee.
                    let final_native = from_native
                        .checked_sub(*fee)
                        .ok_or(TxApplyError::InsufficientBalance { have: from_native, need: *fee })?;
                    out.mutations.push(StateMutation::SetBalance {
                        wallet: *from, token: NATIVE, amount: final_native,
                    });
                } else {
                    let new_from_native = from_native - need_native;
                    let to_native = state.balance_of(to, &NATIVE);
                    let new_to_native = to_native
                        .checked_add(*amount)
                        .ok_or(TxApplyError::Overflow)?;
                    out.mutations.push(StateMutation::SetBalance {
                        wallet: *from, token: NATIVE, amount: new_from_native,
                    });
                    out.mutations.push(StateMutation::SetBalance {
                        wallet: *to, token: NATIVE, amount: new_to_native,
                    });
                }
            } else {
                // Non-native: fee debits NATIVE; `amount` moves in `token`.
                let new_from_native = from_native
                    .checked_sub(*fee)
                    .ok_or(TxApplyError::InsufficientBalance { have: from_native, need: *fee })?;
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: NATIVE, amount: new_from_native,
                });
                if self_send {
                    // (from, token) slot: −amount +amount nets to zero. Emit
                    // nothing for it so the pre-state value is preserved
                    // (balance-sufficiency was checked above).
                } else {
                    let new_from_token = from_token - amount;
                    let to_bal = state.balance_of(to, token);
                    let new_to = to_bal.checked_add(*amount).ok_or(TxApplyError::Overflow)?;
                    out.mutations.push(StateMutation::SetBalance {
                        wallet: *from, token: *token, amount: new_from_token,
                    });
                    out.mutations.push(StateMutation::SetBalance {
                        wallet: *to, token: *token, amount: new_to,
                    });
                }
            }

            // Events: Send on sender side, Receive on recipient side.
            let send_evt = SigilEvent::Send {
                from: *from, to: *to, amount: *amount, token: *token, fee: *fee,
            };
            let recv_evt = SigilEvent::Receive {
                from: *from, to: *to, amount: *amount, token: *token,
            };
            out.mutations.push(StateMutation::PushEventHash(send_evt.leaf_hash()));
            out.mutations.push(StateMutation::PushEventHash(recv_evt.leaf_hash()));
            out.events.push(send_evt);
            out.events.push(recv_evt);
        }

        SigilTx::Swap { from, pool, in_token, in_amt, min_out, fee } => {
            // P5: real constant-product AMM via sigil-dex. The pool's
            // token_a/token_b decides direction; in_token must match one of
            // them. Math + slippage + reserve-floor + k-invariant guards all
            // live in sigil_dex::swap; this layer just routes the snapshot,
            // debits balances, and credits the output.
            let prev_pool = state.pool(pool).ok_or(TxApplyError::PoolNotFound)?.clone();
            let from_native = state.balance_of(from, &NATIVE);
            if from_native < *fee {
                return Err(TxApplyError::InsufficientBalance {
                    have: from_native, need: *fee,
                });
            }

            // Pick direction. Mismatch → loud reject; never silently swap "the
            // other side" because the user typed a token we don't know.
            let (direction, out_token) = if *in_token == prev_pool.token_a {
                (SwapDirection::AtoB, prev_pool.token_b)
            } else if *in_token == prev_pool.token_b {
                (SwapDirection::BtoA, prev_pool.token_a)
            } else {
                return Err(TxApplyError::InvalidSwapToken);
            };

            // Run the pure math.
            let dex_in = dex_pool_from_state(&prev_pool);
            let outcome = sigil_dex::swap(&dex_in, direction, *in_amt, *min_out)?;
            let dex_after = outcome.pool_after;
            let out_amt = outcome.amount_out;

            // Balance dance — sender must hold (fee in NATIVE) + (in_amt in
            // in_token). Same rule as Send: if in_token IS NATIVE, the two
            // sums combine; otherwise they're separate slots.
            let sender_in_bal = state.balance_of(from, in_token);
            if *in_token == NATIVE {
                let need = fee.checked_add(*in_amt).ok_or(TxApplyError::Overflow)?;
                if from_native < need {
                    return Err(TxApplyError::InsufficientBalance {
                        have: from_native, need,
                    });
                }
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: NATIVE, amount: from_native - need,
                });
            } else {
                if sender_in_bal < *in_amt {
                    return Err(TxApplyError::InsufficientBalance {
                        have: sender_in_bal, need: *in_amt,
                    });
                }
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: *in_token, amount: sender_in_bal - in_amt,
                });
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: NATIVE, amount: from_native - fee,
                });
            }

            // Carve out the master-wallet protocol-fee slice (5 bps) from the
            // AMM's output BEFORE crediting the user. If no master wallet is
            // installed yet, `master_share` is 0 and the user receives the
            // full output — same shape as Quillon's pre-bank behavior.
            // See sigil-bank::split_swap_output for the math + rounding policy.
            let split = sigil_bank::split_swap_output(out_amt, state.master_wallet())
                .map_err(|_| TxApplyError::Overflow)?;

            let sender_out_bal = state.balance_of(from, &out_token);
            let new_out_bal = sender_out_bal
                .checked_add(split.user_share)
                .ok_or(TxApplyError::Overflow)?;
            out.mutations.push(StateMutation::SetBalance {
                wallet: *from, token: out_token, amount: new_out_bal,
            });

            // If the master wallet is installed AND the swap was large enough
            // to register at 5 bps resolution, credit the master.
            if let Some(master) = state.master_wallet() {
                if split.master_share > 0 {
                    let master_bal = state.balance_of(&master, &out_token);
                    let new_master_bal = master_bal
                        .checked_add(split.master_share)
                        .ok_or(TxApplyError::Overflow)?;
                    out.mutations.push(StateMutation::SetBalance {
                        wallet: master, token: out_token, amount: new_master_bal,
                    });
                }
            }

            // Persist the pool delta.
            let pool_after = pool_state_from_dex(&prev_pool, &dex_after)?;
            out.mutations.push(StateMutation::SetPool {
                pool: *pool, state: pool_after,
            });

            // Slippage in bps for the event — we know amount_out / min_out
            // satisfy `amount_out >= min_out` from the math, so the actual
            // slippage felt by the user is `(min_out / amount_out) bps off`
            // — but for v0 we just attach the raw values via the event's
            // existing fields.
            let evt = SigilEvent::SwapExecuted {
                pool: *pool,
                in_token: *in_token,
                in_amt: *in_amt,
                out_token,
                out_amt,
                slippage_bps: 0,
                fee_paid: *fee,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }

        SigilTx::LpDeposit { from, pool, token_a, token_b, amt_a, amt_b, fee_bps, fee } => {
            // P5: real LP math via sigil-dex. On first deposit the tx defines
            // the pool's (token_a, token_b, fee_bps). On subsequent deposits
            // we verify the tx mirrors them — mismatch is loud.
            //
            // The per-wallet LP-share credit ledger is NO LONGER deferred: the
            // minted shares are credited to `from` as a balance under
            // `lp_token_id(pool)`, and `LpWithdraw` requires+debits that balance.
            // While it WAS deferred, `LpWithdraw` burned straight out of the
            // pool total, which made every pool drainable by a wallet that had
            // never deposited (PoC 86e7094).

            // Pool-shape check FIRST — caller's clarity about which pool they're
            // hitting is the prerequisite for everything else; balance checks
            // come second so a wrong-pool error doesn't get masked by an
            // insufficient-balance error from the wrong-token side.
            let (prev_pool, outcome) = match state.pool(pool) {
                Some(existing) => {
                    if existing.token_a != *token_a
                        || existing.token_b != *token_b
                        || existing.fee_bps != *fee_bps
                    {
                        return Err(TxApplyError::PoolMismatch);
                    }
                    let dex_in = dex_pool_from_state(existing);
                    let out = sigil_dex::add_liquidity(&dex_in, *amt_a, *amt_b, *fee_bps)?;
                    (existing.clone(), out)
                }
                None => {
                    let empty = DexPool::empty(*fee_bps);
                    let out = sigil_dex::add_liquidity(&empty, *amt_a, *amt_b, *fee_bps)?;
                    let synth = PoolState {
                        token_a: *token_a,
                        token_b: *token_b,
                        reserve_a: 0,
                        reserve_b: 0,
                        lp_shares: 0,
                        fee_bps: *fee_bps,
                        accrued_fees: 0,
                    };
                    (synth, out)
                }
            };

            // Compute the depositor's per-slot final balances FIRST, then
            // emit one SetBalance per touched (wallet, token) pair. This
            // avoids the multi-mutation aliasing bug (two writes to the same
            // slot in one tx — the second silently overwrote the first when
            // token_a or token_b coincided with NATIVE).
            let from_native = state.balance_of(from, &NATIVE);
            let bal_a       = state.balance_of(from, token_a);
            let bal_b       = state.balance_of(from, token_b);

            let mut final_native = from_native;
            let mut final_a      = bal_a;
            let mut final_b      = bal_b;

            // Apply fee debit to NATIVE.
            final_native = final_native
                .checked_sub(*fee)
                .ok_or(TxApplyError::InsufficientBalance { have: from_native, need: *fee })?;
            // Apply amt_a debit. If token_a is NATIVE, this slot is final_native.
            if *token_a == NATIVE {
                final_native = final_native
                    .checked_sub(*amt_a)
                    .ok_or(TxApplyError::InsufficientBalance { have: final_native, need: *amt_a })?;
                final_a = final_native; // same slot
            } else {
                final_a = final_a
                    .checked_sub(*amt_a)
                    .ok_or(TxApplyError::InsufficientBalance { have: bal_a, need: *amt_a })?;
            }
            // Apply amt_b debit. If token_b is NATIVE, again same slot.
            if *token_b == NATIVE {
                final_native = final_native
                    .checked_sub(*amt_b)
                    .ok_or(TxApplyError::InsufficientBalance { have: final_native, need: *amt_b })?;
                final_b = final_native;
            } else {
                final_b = final_b
                    .checked_sub(*amt_b)
                    .ok_or(TxApplyError::InsufficientBalance { have: bal_b, need: *amt_b })?;
            }

            let shares_received = outcome.shares_minted;
            let pool_after = pool_state_from_dex(&prev_pool, &outcome.pool_after)?;

            // LP OWNERSHIP: credit the minted shares to the depositor. This is
            // the ledger `LpWithdraw` checks against; without it the pool total
            // was the ONLY record of shares and anyone could burn against it.
            //
            // The LP slot is a distinct (wallet, token) pair by domain
            // separation, so it cannot alias NATIVE/token_a/token_b and needs no
            // final_* merging like the reserve sides above — but a pool whose
            // own token equals its LP token id would break that, so reject it.
            let lp_tok = lp_token_id(pool);
            if *token_a == lp_tok || *token_b == lp_tok {
                return Err(TxApplyError::LpTokenCollision);
            }
            let final_lp = state
                .balance_of(from, &lp_tok)
                .checked_add(shares_received)
                .ok_or(TxApplyError::Overflow)?;

            // Emit one SetBalance per unique slot. NATIVE comes from
            // final_native; token_a/token_b only get written if they're
            // distinct from NATIVE (otherwise final_native already captures
            // them).
            out.mutations.push(StateMutation::SetBalance {
                wallet: *from, token: NATIVE, amount: final_native,
            });
            if *token_a != NATIVE {
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: *token_a, amount: final_a,
                });
            }
            if *token_b != NATIVE && *token_b != *token_a {
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: *token_b, amount: final_b,
                });
            }
            out.mutations.push(StateMutation::SetBalance {
                wallet: *from, token: lp_tok, amount: final_lp,
            });
            out.mutations.push(StateMutation::SetPool { pool: *pool, state: pool_after });

            let evt = SigilEvent::LpDeposited {
                pool: *pool, amt_a: *amt_a, amt_b: *amt_b, shares_received,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::LpWithdraw { from, pool, shares, fee } => {
            let prev_pool = state.pool(pool).ok_or(TxApplyError::PoolNotFound)?.clone();
            let from_native = state.balance_of(from, &NATIVE);
            if from_native < *fee {
                return Err(TxApplyError::InsufficientBalance {
                    have: from_native, need: *fee,
                });
            }

            // LP OWNERSHIP GATE — the fix for the drain (PoC 86e7094).
            //
            // This check is what stops a wallet burning shares it never had.
            // It runs BEFORE the AMM math so a thief gets `InsufficientLpShares`
            // (the true reason) rather than a downstream dex error, and so no
            // payout is computed for a burn that was never authorised.
            let lp_tok = lp_token_id(pool);
            if prev_pool.token_a == lp_tok || prev_pool.token_b == lp_tok {
                return Err(TxApplyError::LpTokenCollision);
            }
            let lp_owned = state.balance_of(from, &lp_tok);
            if lp_owned < *shares {
                return Err(TxApplyError::InsufficientLpShares {
                    have: lp_owned, need: *shares,
                });
            }
            let final_lp = lp_owned - *shares; // checked by the guard above

            let dex_in = dex_pool_from_state(&prev_pool);
            let outcome = sigil_dex::remove_liquidity(&dex_in, *shares)?;
            let pool_after = pool_state_from_dex(&prev_pool, &outcome.pool_after)?;

            // Compute final per-slot balances, then emit one SetBalance per
            // unique slot — same aliasing-safe pattern as LpDeposit.
            let bal_a = state.balance_of(from, &prev_pool.token_a);
            let bal_b = state.balance_of(from, &prev_pool.token_b);

            let mut final_native = from_native
                .checked_sub(*fee)
                .ok_or(TxApplyError::InsufficientBalance { have: from_native, need: *fee })?;
            let mut final_a = bal_a;
            let mut final_b = bal_b;
            if prev_pool.token_a == NATIVE {
                final_native = final_native
                    .checked_add(outcome.amount_a)
                    .ok_or(TxApplyError::Overflow)?;
                final_a = final_native;
            } else {
                final_a = bal_a
                    .checked_add(outcome.amount_a)
                    .ok_or(TxApplyError::Overflow)?;
            }
            if prev_pool.token_b == NATIVE {
                final_native = final_native
                    .checked_add(outcome.amount_b)
                    .ok_or(TxApplyError::Overflow)?;
                final_b = final_native;
            } else {
                final_b = bal_b
                    .checked_add(outcome.amount_b)
                    .ok_or(TxApplyError::Overflow)?;
            }

            out.mutations.push(StateMutation::SetBalance {
                wallet: *from, token: NATIVE, amount: final_native,
            });
            if prev_pool.token_a != NATIVE {
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: prev_pool.token_a, amount: final_a,
                });
            }
            if prev_pool.token_b != NATIVE && prev_pool.token_b != prev_pool.token_a {
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: prev_pool.token_b, amount: final_b,
                });
            }
            // Debit the burned shares from the withdrawer's LP ledger, so the
            // per-wallet total tracks the pool total the burn just reduced.
            out.mutations.push(StateMutation::SetBalance {
                wallet: *from, token: lp_tok, amount: final_lp,
            });
            out.mutations.push(StateMutation::SetPool { pool: *pool, state: pool_after });

            let evt = SigilEvent::LpWithdrawn {
                pool: *pool, shares_burned: *shares,
                amt_a: outcome.amount_a,
                amt_b: outcome.amount_b,
                fees_realized: 0,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::ContractCall { contract, method, .. } => {
            let evt = SigilEvent::ContractCall {
                contract: *contract, method: *method,
                gas_used: 0, result_hash: [0u8; 32],
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::ContractDeploy { from, bytecode, .. } => {
            let bytecode_hash: [u8; 32] = *blake3::hash(bytecode).as_bytes();
            let contract_id: [u8; 32] = *blake3::hash(&[&from[..], bytecode].concat()).as_bytes();
            let evt = SigilEvent::ContractDeploy {
                creator: *from, contract_id, bytecode_hash, gas_used: 0,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::TokenDeploy { creator, ticker, decimals, initial_supply, .. } => {
            let evt = SigilEvent::TokenDeployed {
                creator: *creator, ticker: ticker.clone(),
                decimals: *decimals, initial_supply: *initial_supply,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::ValidatorJoin { validator, stake, .. } => {
            let evt = SigilEvent::ValidatorJoined {
                validator: *validator, stake: *stake,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::ValidatorLeave { validator, .. } => {
            let evt = SigilEvent::ValidatorLeft {
                validator: *validator, refunded_stake: 0,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::UsdsMint { from, sigil_amount, .. } => {
            // All the math (buffer + protocol fee) lives in sigil_usds::plan_mint
            // — same "pure planner, caller commits" shape sigil_dex::swap
            // already uses for SigilTx::Swap above.
            let plan = sigil_usds::plan_mint(state, *from, *sigil_amount)?;
            let evt = SigilEvent::UsdsMinted {
                wallet: *from, sigil_locked: *sigil_amount, usds_minted: plan.usds_to_user,
            };
            out.mutations.extend(plan.mutations);
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::UsdsRedeem { from, usds_amount, .. } => {
            let plan = sigil_usds::plan_redeem(state, *from, *usds_amount)?;
            let evt = SigilEvent::UsdsRedeemed {
                wallet: *from, usds_burned: *usds_amount, sigil_released: plan.sigil_to_user,
            };
            out.mutations.extend(plan.mutations);
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::MandateCreate { id, agent, max_amount, purpose, created_ts, expires_ts, .. } => {
            // Event-only, no SigilState storage change — same shape as
            // TokenDeploy/ValidatorJoin above. The queryable MandateBook is
            // reconstructed OUTSIDE SigilState by folding MandateCreated/
            // MandateClosed events (sigil_bank::mandate::fold_events), same
            // as the validator-set kinds are documented to do once ported.
            let evt = SigilEvent::MandateCreated {
                id: id.clone(), agent: *agent, max_amount: *max_amount,
                purpose: purpose.clone(), created_ts: *created_ts, expires_ts: *expires_ts,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::MandateClose { id, agent, .. } => {
            // Ownership (this agent actually holds mandate `id`) is checked
            // by the caller against its replayed MandateBook BEFORE
            // submitting the tx — apply_tx has no access to that book (it
            // is not SigilState), so it cannot re-check it here. This
            // mirrors every other kind in this function: apply_tx trusts
            // precheck() (signature) and emits; higher-level authorization
            // is the caller's job, same as the existing `authorize()` gate
            // in sigil-rpcd today.
            let evt = SigilEvent::MandateClosed { id: id.clone(), agent: *agent };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::BankPropose { id, from, to, token, amount, proposer, created_ts, .. } => {
            // Event-only, same reasoning as MandateCreate: the replayed
            // Council (not SigilState) is where "does this exist / who has
            // approved" lives — apply_tx just records the fact it happened.
            let evt = SigilEvent::BankProposed {
                id: id.clone(), from: *from, to: *to, token: *token, amount: *amount,
                proposer: *proposer, created_ts: *created_ts,
            };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::BankApprove { id, approver, .. } => {
            // Event-only — see BankPropose. Whether this reaches threshold
            // is the caller's question, not apply_tx's.
            let evt = SigilEvent::BankApproved { id: id.clone(), approver: *approver };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }
        SigilTx::BankExecute { id, from, to, token, amount, .. } => {
            // Real balance movement — same aliasing-safe shape as `Send`
            // (single unique-slot debit when from==to, so a degenerate
            // self-transfer nets to zero instead of minting `amount`), but
            // without Send's native-fee bookkeeping: the CURRENT live
            // behavior this replaces (sigil-rpcd's /bank/approve execute
            // step) never charged one either — `fee` on this variant is
            // reserved for a future policy, not enforced here.
            let from_bal = state.balance_of(from, token);
            if from_bal < *amount {
                return Err(TxApplyError::InsufficientBalance { have: from_bal, need: *amount });
            }
            if from == to {
                // Debit and credit cancel on the same slot — no-op transfer,
                // net zero, matches Send's self-send handling.
            } else {
                let new_from = from_bal - *amount;
                let to_bal = state.balance_of(to, token);
                let new_to = to_bal.checked_add(*amount).ok_or(TxApplyError::Overflow)?;
                out.mutations.push(StateMutation::SetBalance { wallet: *from, token: *token, amount: new_from });
                out.mutations.push(StateMutation::SetBalance { wallet: *to, token: *token, amount: new_to });
            }
            let evt = SigilEvent::BankExecuted { id: id.clone(), from: *from, to: *to, token: *token, amount: *amount };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }

        // ── SIGIL-Nation: citizenship + welfare ─────────────────────────────
        // Both arms use `at_height.unwrap_or(0)`: the legacy no-height entry
        // point therefore REFUSES nation txs (0 < activation height) instead
        // of applying them ungated — stricter than the multi-input-spend
        // convention on purpose, because these arms also need the real height
        // as DATA (the claim ledger records it), not just as a gate.
        SigilTx::CitizenAttest { authority, citizen, cpr_hash, fee } => {
            use sigil_bank::welfare as wf;
            let height = at_height.unwrap_or(0);
            if !wf::welfare_active(height) {
                return Err(TxApplyError::NationNotActive { height, activates_at: wf::WELFARE_FROM_HEIGHT });
            }
            // The nation authority IS the genesis-committed master wallet —
            // the only genesis-named wallet with a real keyholder. The
            // legacy BORGER_AUTHORITY placeholder ([0x0A;32]) has no keys,
            // so binding to it would make attestation permanently unusable.
            match state.master_wallet() {
                Some(m) if m == *authority => {}
                _ => return Err(TxApplyError::NotNationAuthority),
            }
            if cpr_hash == &[0u8; 32] {
                return Err(TxApplyError::InvalidAttestation);
            }
            let have = state.balance_of(authority, &NATIVE);
            if have < *fee {
                return Err(TxApplyError::InsufficientBalance { have, need: *fee });
            }
            if *fee > 0 {
                // Fee burns, same as Send: debited, credited nowhere.
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *authority, token: NATIVE, amount: have - *fee,
                });
            }
            out.mutations.push(StateMutation::SetContractSlot {
                contract: wf::BORGER_REGISTRY, slot: *citizen, value: *cpr_hash,
            });
            let evt = SigilEvent::CitizenAttested { authority: *authority, citizen: *citizen };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }

        SigilTx::WelfareClaim { citizen, fee } => {
            use sigil_bank::welfare as wf;
            let height = at_height.unwrap_or(0);
            if !wf::welfare_active(height) {
                return Err(TxApplyError::NationNotActive { height, activates_at: wf::WELFARE_FROM_HEIGHT });
            }
            // Degenerate aliasing guard: the treasury itself can never be a
            // claimant — a claim would debit and credit the same slot.
            if *citizen == wf::WELFARE_WALLET {
                return Err(TxApplyError::NotCitizen);
            }
            if state.contract_slot(&wf::BORGER_REGISTRY, citizen) == [0u8; 32] {
                return Err(TxApplyError::NotCitizen);
            }
            let last = wf::decode_claim_height(&state.contract_slot(&wf::WELFARE_LEDGER, citizen));
            if !wf::claim_eligible(last, height) {
                return Err(TxApplyError::WelfareCooldown { next_height: wf::next_claim_height(last) });
            }
            // ── sUSD payout (operator ruling 2026-08-31) ─────────────────
            // The stipend is DENOMINATED IN DOLLARS and PAID IN USDS: the
            // treasury's SIGIL is locked into the USDS vault as collateral
            // (105% buffer, oracle-priced) and exactly
            // WELFARE_STIPEND_USD_E8 of freshly minted USDS lands in the
            // citizen's wallet — volatility stays with the treasury, never
            // the citizen. The tx fee still burns (folded into the SAME
            // treasury debit — two absolute SetBalance writes to one key
            // would be last-write-wins) and still cannot exceed one SIGIL
            // (WELFARE_STIPEND_GLYPHS, now purely the fee ceiling), so a
            // zero-balance citizen can always claim. No oracle price →
            // UsdsError::NoPrice → the claim REFUSES: fail closed, never an
            // unbacked payment.
            if *fee > wf::WELFARE_STIPEND_GLYPHS {
                return Err(TxApplyError::InsufficientBalance { have: wf::WELFARE_STIPEND_GLYPHS, need: *fee });
            }
            let plan = match sigil_usds::plan_welfare_mint(
                state, wf::WELFARE_WALLET, *citizen, wf::WELFARE_STIPEND_USD_E8, *fee,
            ) {
                Ok(p) => p,
                Err(sigil_usds::UsdsError::WelfarePayerUnderfunded { have, need }) => {
                    return Err(TxApplyError::WelfareTreasuryInsufficient { have, need });
                }
                Err(e) => return Err(e.into()),
            };
            out.mutations.extend(plan.mutations);
            out.mutations.push(StateMutation::SetContractSlot {
                contract: wf::WELFARE_LEDGER, slot: *citizen, value: wf::encode_claim_height(height),
            });
            // `amount` is USDS base units (1e8 = $1) since the sUSD payout —
            // pre-USDS it was glyphs; no claim landed under the old meaning
            // (activation-gated), so the event stream stays single-meaning.
            let evt = SigilEvent::WelfareClaimed { citizen: *citizen, amount: plan.usds_to_recipient };
            out.mutations.push(StateMutation::PushEventHash(evt.leaf_hash()));
            out.events.push(evt);
        }

        SigilTx::OraclePush { authority, price_usd_e8, fee } => {
            use sigil_bank::welfare as wf;
            let height = at_height.unwrap_or(0);
            if !wf::welfare_active(height) {
                return Err(TxApplyError::NationNotActive { height, activates_at: wf::WELFARE_FROM_HEIGHT });
            }
            // Same authority rule as CitizenAttest: the state-committed
            // master wallet, because the genesis ORACLE_AUTHORITY
            // placeholder has no keyholder.
            match state.master_wallet() {
                Some(m) if m == *authority => {}
                _ => return Err(TxApplyError::NotNationAuthority),
            }
            if *price_usd_e8 == 0 {
                return Err(TxApplyError::ZeroOraclePrice);
            }
            let have = state.balance_of(authority, &NATIVE);
            if have < *fee {
                return Err(TxApplyError::InsufficientBalance { have, need: *fee });
            }
            if *fee > 0 {
                // Fee burns, same as CitizenAttest.
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *authority, token: NATIVE, amount: have - *fee,
                });
            }
            // Byte-identical encoding to sigil_oracle::update_price, so
            // read_price sees exactly what a direct push would have written.
            let mut value = [0u8; 32];
            value[..16].copy_from_slice(&price_usd_e8.to_le_bytes());
            out.mutations.push(StateMutation::SetContractSlot {
                contract: sigil_oracle::ORACLE_CONTRACT, slot: sigil_oracle::PRICE_SLOT, value,
            });
        }
    }

    Ok(out)
}

/// Combine N applied txs into a single block-shaped [`StateTransition`].
/// Caller passes the height the transition will seal into.
pub fn batch_into_transition(
    results: impl IntoIterator<Item = ApplyResult>,
    at_height: u64,
) -> StateTransition {
    let mut mutations = Vec::new();
    for r in results {
        mutations.extend(r.mutations);
    }
    StateTransition { at_height, mutations }
}

/// R2: apply a whole [`AuthorizedBatch`] to state — the primitive the producer
/// mint and EVERY verifier call, so the one-signature-per-batch amortization
/// reaches all of them. Verifies the ONE signature (author + R0 nonce binding),
/// enforces the R0 consensus replay nonce, applies every op via the real
/// [`apply_tx`] (which does zero signature work — the batch sig authorized them),
/// and commits ONE transition. ATOMIC: on a replay or an op failure, state is
/// unchanged and the author's nonce does not advance.
pub fn apply_authorized_batch(
    state: &mut sigil_state::SigilState,
    batch: &AuthorizedBatch,
    at_height: u64,
) -> Result<sigil_state::StateRoots, sigil_state::CommitError> {
    use sigil_state::CommitError;
    // 1. the ONE signature + single-author + nonce binding.
    batch
        .verify()
        .map_err(|e| CommitError::Invariant(format!("batch verify: {e}")))?;
    // 2. consensus replay guard — READ the high-water first; mutate nothing until commit.
    let cur = state.nonce_of(&batch.author);
    if batch.nonce <= cur {
        return Err(CommitError::NonceReplay { got: batch.nonce, have: cur });
    }
    // 3. compute every op's mutations via the real apply_tx (read-only on state).
    let mut mutations = Vec::new();
    for op in &batch.ops {
        let signed = wrap_op(op.clone());
        let r = apply_tx(state, &signed)
            .map_err(|e| CommitError::Invariant(format!("apply op: {e}")))?;
        mutations.extend(r.mutations);
    }
    // 4. commit the ops, THEN advance the nonce — so a commit failure leaves the
    //    nonce untouched and the whole batch is cleanly retryable.
    sigil_state::commit_state_transition(state, &StateTransition { at_height, mutations }, at_height)?;
    state.check_and_bump_nonce(&batch.author, batch.nonce)?;
    Ok(state.roots())
}

/// Wrap a bare (batch-authorized) op as a [`SignedTx`] for [`apply_tx`], which
/// does ZERO signature work — the batch signature already authorized the op. The
/// dummy signature is only length-checked by `precheck`.
fn wrap_op(op: SigilTx) -> SignedTx {
    let payer = op.fee_payer();
    SignedTx {
        tx: op,
        from_pubkey: payer,
        nonce: 0,
        sig_scheme: SigScheme::Ed25519Hot,
        sig: SignatureBytes(vec![0u8; SigScheme::Ed25519Hot.expected_sig_len()]),
        pubkey: PubKeyBytes(Vec::new()),
    }
}

// ── Constants ───────────────────────────────────────────────────────────────

/// All-zero token ID = native SIGIL.
pub const NATIVE: TokenId = [0u8; 32];

/// Domain-separation tag for LP-share token ids. Distinct from every other
/// BLAKE3 use in the tx layer so an LP slot can never be confused with a
/// deployed token, a wallet, or a pool id.
const LP_SHARE_DOMAIN: &[u8] = b"sigil-tx/lp-share/v1";

/// The [`TokenId`] under which a pool's LP shares are held, per wallet.
///
/// ## Why LP shares are a TOKEN and not a new state map
///
/// `SigilState` already stores balances as `(WalletId, TokenId) -> u128` —
/// multi-token by construction — and `wallet_acc` already folds every one of
/// those entries into `wallet_state_root` in O(1). Deriving an LP token id per
/// pool therefore gives per-wallet LP accounting with:
///   * **no new state map**, so no new root and no schema bump;
///   * **no new `StateMutation` variant**, so the chokepoint and any future
///     STARK circuit see ordinary `SetBalance`es they already know how to
///     verify and conserve;
///   * **no change to `sigil-state` at all** — the fix lands entirely in the
///     tx layer.
///
/// It also matches how production AMMs model this (Uniswap LP positions are
/// themselves ERC-20 tokens), which makes LP positions transferable by the
/// ordinary `Send` path for free rather than needing a bespoke transfer tx.
///
/// The output is a BLAKE3 hash, so it cannot equal [`NATIVE`] (`[0u8; 32]`)
/// short of a preimage break, and two distinct pools cannot share an LP token.
pub fn lp_token_id(pool: &PoolId) -> TokenId {
    let mut h = blake3::Hasher::new();
    h.update(LP_SHARE_DOMAIN);
    h.update(pool);
    *h.finalize().as_bytes()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_header::{SqiSignature, SQISIGN_L5_LEN};
    use sigil_state::commit_state_transition;

    /// The gate must be INERT until an operator schedules it, and must never affect a
    /// wallet that did not opt in. Both halves matter: a premature activation forks the
    /// chain, and conscripting non-upgraded wallets would break every existing user.
    #[test]
    fn sqi_ramp_gate_is_dormant_and_only_binds_opted_in_wallets() {
        assert_eq!(SQI_RAMP_REQUIRED_HEIGHT, u64::MAX, "must ship DORMANT");

        // Dormant: nothing is required at any reachable height, key or no key.
        assert!(!sqi_ramp_required(0, true));
        assert!(!sqi_ramp_required(1_000_000, true));
        assert!(!sqi_ramp_required(u64::MAX - 1, true));

        // A wallet with no registered key is never affected, even at activation.
        assert!(!sqi_ramp_required(u64::MAX, false));

        // And at activation, a wallet that DID opt in is bound.
        assert!(sqi_ramp_required(u64::MAX, true));
    }

    fn dummy_signed(tx: SigilTx) -> SignedTx {
        let from = tx.fee_payer();
        SignedTx {
            tx,
            from_pubkey: from,
            nonce: 0,
            sig_scheme: SigScheme::SqiSign5,
            sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
            // apply_tx only prechecks — these helpers never call verify_signature.
            pubkey: PubKeyBytes(Vec::new()),
        }
    }

    /// Real SQIsign Level-5 roundtrip through the new verify path: keygen →
    /// derive the bound WalletId → sign tx.hash() → verify (single + batch),
    /// then prove a tampered tx is rejected. This is the Tier-1 sig-wall gate
    /// exercised end-to-end with real post-quantum crypto.
    #[test]
    fn verify_signature_real_sqisign_and_batch() {
        let (sk, pk) = flux_sqisign::keygen(); // (sk, pk)
        assert_eq!(pk.len(), SigScheme::SqiSign5.expected_pubkey_len());
        let from = wallet_id_from_pubkey(&pk);
        let tx = SigilTx::Send { from, to: [9u8; 32], amount: 10, token: NATIVE, fee: 1 };
        let sig = flux_sqisign::sign(&tx.hash(), &sk, &pk).expect("sign");
        assert_eq!(sig.len(), SigScheme::SqiSign5.expected_sig_len());
        let signed = SignedTx {
            tx,
            from_pubkey: from,
            nonce: 0,
            sig_scheme: SigScheme::SqiSign5,
            sig: SignatureBytes(sig),
            pubkey: PubKeyBytes(pk),
        };
        // valid: single + parallel batch both accept.
        signed.verify_signature().expect("valid sig must verify");
        verify_batch_parallel(std::slice::from_ref(&signed)).expect("batch must accept");

        // tampered intent → digest changes → signature no longer valid.
        let mut bad = signed.clone();
        bad.tx = SigilTx::Send { from, to: [9u8; 32], amount: 11, token: NATIVE, fee: 1 };
        assert!(matches!(bad.verify_signature(), Err(TxApplyError::SignatureInvalid)));

        // wrong pubkey length → rejected before any curve op.
        let mut shortpk = signed.clone();
        shortpk.pubkey = PubKeyBytes(vec![0u8; 10]);
        assert!(matches!(
            shortpk.verify_signature(),
            Err(TxApplyError::PubKeyLengthMismatch { .. })
        ));

        // batch surfaces the lowest failing index.
        let batch = vec![signed.clone(), bad.clone()];
        match verify_batch_parallel(&batch) {
            Err((idx, TxApplyError::SignatureInvalid)) => assert_eq!(idx, 1),
            other => panic!("expected idx-1 SignatureInvalid, got {other:?}"),
        }
    }

    /// Ed25519 hot-path + verify-once Mempool end-to-end: keygen → sign → ingest
    /// verifies ONCE (batch-MSM fast path) → pull does NOT re-verify; a tampered
    /// sig is dropped at ingest, and re-ingesting accepted txs is all-dupe with
    /// ZERO extra verification (the verify-once invariant, asserted via the meter).
    #[test]
    fn mempool_verify_once_ed25519() {
        // 64 valid ed25519 txs, each independently verifiable.
        let mut txs = Vec::new();
        for i in 0..64u64 {
            let (sk, pk, from) = ed25519_keygen();
            let tx = SigilTx::Send { from, to: [7u8; 32], amount: u128::from(i) + 1, token: NATIVE, fee: 1 };
            let signed = ed25519_sign_tx(tx, &sk, &pk);
            signed.verify_signature().expect("valid ed25519 tx must verify");
            txs.push(signed);
        }
        // one tampered tx: sign, then mutate the intent so the digest no longer matches.
        let (sk, pk, from) = ed25519_keygen();
        let mut bad = ed25519_sign_tx(
            SigilTx::Send { from, to: [7u8; 32], amount: 99, token: NATIVE, fee: 1 }, &sk, &pk);
        bad.tx = SigilTx::Send { from, to: [7u8; 32], amount: 100, token: NATIVE, fee: 1 };
        assert!(bad.verify_signature().is_err(), "tampered tx must fail");

        // ingest a mixed batch: 64 valid + 1 invalid.
        let mut mp = Mempool::new();
        let mut mixed = txs.clone();
        mixed.push(bad);
        let r = mp.ingest(mixed);
        assert_eq!(r.accepted, 64);
        assert_eq!(r.invalid, 1);
        assert_eq!(r.dupe, 0);
        assert_eq!(mp.len(), 64);
        assert_eq!(mp.verified_total(), 64, "exactly 64 sigs verified on first ingest");

        // re-ingest the accepted txs: all dupes, and the verify meter does NOT move
        // (they are never re-verified — the whole point of verify-once).
        let r2 = mp.ingest(txs.clone());
        assert_eq!(r2.dupe, 64);
        assert_eq!(r2.accepted, 0);
        assert_eq!(mp.verified_total(), 64, "verify-once: re-ingest must not re-verify");

        // pull hands verified txs to the producer without re-verification.
        let pulled = mp.pull(40);
        assert_eq!(pulled.len(), 40);
        assert_eq!(mp.len(), 24);
    }

    /// AuthorizedBatch: ONE signature authorizes N ops. Verify accepts a valid
    /// single-author batch; rejects a tampered op (root changes), an added op,
    /// and a cross-author op (someone else's funds) — the soundness boundary.
    #[test]
    fn authorized_batch_one_sig_n_ops() {
        let (sk, pk, author) = ed25519_keygen();
        let mk = |amount: u128| SigilTx::Send { from: author, to: [9u8; 32], amount, token: NATIVE, fee: 1 };
        let ops: Vec<SigilTx> = (1..=500u128).map(mk).collect();

        // valid batch of 500 ops, ONE signature → verifies.
        let batch = AuthorizedBatch::sign_ed25519(ops.clone(), 1, &sk, &pk);
        assert_eq!(batch.len(), 500);
        batch.verify().expect("valid single-author batch must verify");

        // tamper one op → root changes → sig no longer matches.
        let mut tampered = batch.clone();
        tampered.ops[123] = mk(999_999);
        assert!(matches!(tampered.verify(), Err(TxApplyError::SignatureInvalid)));

        // append an op the author never signed → root changes → rejected.
        let mut added = batch.clone();
        added.ops.push(mk(7));
        assert!(matches!(added.verify(), Err(TxApplyError::SignatureInvalid)));

        // a cross-author op (moves someone else's funds) → SignerNotPayer.
        let (_sk2, _pk2, other) = ed25519_keygen();
        let mut cross = ops.clone();
        cross.push(SigilTx::Send { from: other, to: author, amount: 1, token: NATIVE, fee: 1 });
        // (re-sign so the sig matches the new root; the author-binding check still fires)
        let cross_batch = AuthorizedBatch::sign_ed25519(cross, 1, &sk, &pk);
        assert!(matches!(cross_batch.verify(), Err(TxApplyError::SignerNotPayer)));
    }

    fn fund(state: &mut SigilState, wallet: WalletId, amount: u128) {
        let t = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance {
                wallet, token: NATIVE, amount,
            }],
        };
        commit_state_transition(state, &t, 0).unwrap();
    }

    #[test]
    fn send_changes_wallet_root_and_emits_two_events() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let bob:   WalletId = [2u8; 32];
        fund(&mut s, alice, 1_000);

        let pre = s.roots().wallet_state_root;

        let signed = dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 100, token: NATIVE, fee: 1,
        });
        let result = apply_tx(&s, &signed).unwrap();
        assert_eq!(result.events.len(), 2, "Send + Receive expected");

        let transition = batch_into_transition([result], 1);
        commit_state_transition(&mut s, &transition, 1).unwrap();

        assert_eq!(s.balance_of(&alice, &NATIVE), 1_000 - 100 - 1);
        assert_eq!(s.balance_of(&bob,   &NATIVE), 100);
        assert_ne!(s.roots().wallet_state_root, pre);
    }

    /// T6 PROOF — two INDEPENDENTLY constructed nodes, fed the same
    /// MandateCreate then MandateClose txs (never sharing any state or
    /// process), converge to byte-identical MandateBooks AND identical
    /// `event_log_root`s. This is the actual claim T6 needs: not "the
    /// function is deterministic in isolation" but "two separate replays
    /// of the same chain history land on the same answer" — the property
    /// rpcd's old local-only `n.mandates` write never had.
    #[test]
    fn mandate_create_close_converges_across_two_independent_nodes() {
        let agent: WalletId = [9u8; 32];
        let create = dummy_signed(SigilTx::MandateCreate {
            id: "m1".into(), agent, max_amount: 1_000, purpose: "trade".into(),
            created_ts: 1_000, expires_ts: 2_000, fee: 0,
        });
        let close = dummy_signed(SigilTx::MandateClose { id: "m1".into(), agent, fee: 0 });

        // Node A and Node B never touch each other — same tx bytes, applied
        // independently, is the whole point of the proof.
        let mut node_a = SigilState::new();
        let mut node_b = SigilState::new();

        let ra1 = apply_tx(&node_a, &create).unwrap();
        assert_eq!(ra1.events.len(), 1);
        commit_state_transition(&mut node_a, &batch_into_transition([ra1.clone()], 0), 0).unwrap();
        let rb1 = apply_tx(&node_b, &create).unwrap();
        commit_state_transition(&mut node_b, &batch_into_transition([rb1.clone()], 0), 0).unwrap();

        // Post-create: books converge, and so does the on-chain event root.
        let book_a = sigil_bank::mandate::fold_events(ra1.events.iter());
        let book_b = sigil_bank::mandate::fold_events(rb1.events.iter());
        assert_eq!(book_a, book_b);
        assert_eq!(node_a.roots().event_log_root, node_b.roots().event_log_root);
        let m = book_a.get("m1").expect("mandate m1 must exist after fold");
        assert_eq!(m.agent, agent);
        assert_eq!(m.max_amount, 1_000);
        assert_eq!(m.purpose, "trade");
        assert_eq!(m.created_ts, 1_000);
        assert_eq!(m.expires_ts, 2_000);
        assert!(m.is_live(1_500), "live between created_ts and expires_ts");
        assert!(!m.is_live(2_500), "not live past expires_ts");

        // Now close it on both nodes independently, replay the FULL event
        // history (create + close) on each, and confirm they still agree.
        let ra2 = apply_tx(&node_a, &close).unwrap();
        commit_state_transition(&mut node_a, &batch_into_transition([ra2.clone()], 1), 1).unwrap();
        let rb2 = apply_tx(&node_b, &close).unwrap();
        commit_state_transition(&mut node_b, &batch_into_transition([rb2.clone()], 1), 1).unwrap();

        let all_a: Vec<_> = ra1.events.iter().chain(ra2.events.iter()).collect();
        let all_b: Vec<_> = rb1.events.iter().chain(rb2.events.iter()).collect();
        let final_a = sigil_bank::mandate::fold_events(all_a.into_iter());
        let final_b = sigil_bank::mandate::fold_events(all_b.into_iter());
        assert_eq!(final_a, final_b);
        assert_eq!(node_a.roots().event_log_root, node_b.roots().event_log_root);
        assert_eq!(final_a.get("m1").unwrap().status, "closed");
        assert!(!final_a.get("m1").unwrap().is_live(1_500), "closed authorizes nothing even before expiry");
    }

    /// T6 PROOF (bank council) — propose, first approval (not yet at
    /// threshold), second approval (reaches 2-of-2, so the caller also
    /// submits BankExecute — same two-step shape rpcd's handler uses: one
    /// apply for the approval event, a second for the resulting transfer).
    /// Run independently on two nodes that never share state; assert both
    /// the replayed Council AND the actual moved balances converge.
    #[test]
    fn bank_propose_approve_execute_converges_across_two_independent_nodes() {
        let treasury: WalletId = [4u8; 32];
        let m1: WalletId = [5u8; 32];
        let m2: WalletId = [6u8; 32];
        let recipient: WalletId = [7u8; 32];

        let propose = dummy_signed(SigilTx::BankPropose {
            id: "p1".into(), from: treasury, to: recipient, token: NATIVE, amount: 300,
            proposer: m1, created_ts: 1_000, fee: 0,
        });
        let approve1 = dummy_signed(SigilTx::BankApprove { id: "p1".into(), approver: m1, fee: 0 });
        let approve2 = dummy_signed(SigilTx::BankApprove { id: "p1".into(), approver: m2, fee: 0 });
        let execute = dummy_signed(SigilTx::BankExecute {
            id: "p1".into(), from: treasury, to: recipient, token: NATIVE, amount: 300,
            executor: m2, fee: 0,
        });

        let mut node_a = SigilState::new();
        let mut node_b = SigilState::new();
        fund(&mut node_a, treasury, 1_000);
        fund(&mut node_b, treasury, 1_000);

        let mut events_a = Vec::new();
        let mut events_b = Vec::new();
        let mut h_a = 1u64;
        let mut h_b = 1u64;
        for tx in [&propose, &approve1] {
            let ra = apply_tx(&node_a, tx).unwrap();
            commit_state_transition(&mut node_a, &batch_into_transition([ra.clone()], h_a), h_a).unwrap();
            events_a.extend(ra.events);
            h_a += 1;
            let rb = apply_tx(&node_b, tx).unwrap();
            commit_state_transition(&mut node_b, &batch_into_transition([rb.clone()], h_b), h_b).unwrap();
            events_b.extend(rb.events);
            h_b += 1;
        }

        // Each node independently checks ITS OWN replayed Council for
        // threshold — same check rpcd's handler performs after approve2.
        let mut council_a = sigil_bank::council::fold_events(events_a.iter());
        council_a.seed(vec![m1, m2], 2);
        let ready_a = council_a.approve("p1", m2).unwrap();
        assert!(ready_a, "second approval must reach 2-of-2");

        // Now apply approve2 + execute for real on both nodes.
        for tx in [&approve2, &execute] {
            let ra = apply_tx(&node_a, tx).unwrap();
            commit_state_transition(&mut node_a, &batch_into_transition([ra.clone()], h_a), h_a).unwrap();
            events_a.extend(ra.events);
            h_a += 1;
            let rb = apply_tx(&node_b, tx).unwrap();
            commit_state_transition(&mut node_b, &batch_into_transition([rb.clone()], h_b), h_b).unwrap();
            events_b.extend(rb.events);
            h_b += 1;
        }

        let mut final_a = sigil_bank::council::fold_events(events_a.iter());
        let mut final_b = sigil_bank::council::fold_events(events_b.iter());
        final_a.seed(vec![m1, m2], 2);
        final_b.seed(vec![m1, m2], 2);
        assert_eq!(final_a, final_b);
        assert_eq!(final_a.get("p1").unwrap().status, "executed");
        assert_eq!(final_a.get("p1").unwrap().approvals.len(), 2);

        // The money side converges too — not just the paperwork.
        assert_eq!(node_a.balance_of(&treasury, &NATIVE), 700);
        assert_eq!(node_a.balance_of(&recipient, &NATIVE), 300);
        assert_eq!(node_b.balance_of(&treasury, &NATIVE), 700);
        assert_eq!(node_b.balance_of(&recipient, &NATIVE), 300);
        assert_eq!(node_a.roots().wallet_state_root, node_b.roots().wallet_state_root);
    }

    /// A degenerate proposal where `from == to` must net to zero, not mint
    /// `amount` — same aliasing class Send/LpDeposit already guard against.
    #[test]
    fn bank_execute_self_transfer_does_not_mint() {
        let treasury: WalletId = [4u8; 32];
        let mut s = SigilState::new();
        fund(&mut s, treasury, 1_000);
        let tx = dummy_signed(SigilTx::BankExecute {
            id: "p2".into(), from: treasury, to: treasury, token: NATIVE, amount: 400,
            executor: [6u8; 32], fee: 0,
        });
        let result = apply_tx(&s, &tx).unwrap();
        assert!(result.mutations.iter().all(|m| !matches!(m, StateMutation::SetBalance { .. })),
            "self-transfer must emit no SetBalance at all, not a cancelling pair");
        commit_state_transition(&mut s, &batch_into_transition([result], 1), 1).unwrap();
        assert_eq!(s.balance_of(&treasury, &NATIVE), 1_000, "unchanged — no mint, no burn");
    }

    #[test]
    fn self_send_native_does_not_mint() {
        // Regression: a Send with from == to (native) must net to −fee, NOT
        // mint `amount`. Pre-fix, the recipient credit overwrote the sender
        // debit on the same slot → balance = start + amount (free money).
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        fund(&mut s, alice, 1_000);
        let supply_before = s.native_supply();

        let signed = dummy_signed(SigilTx::Send {
            from: alice, to: alice, amount: 500, token: NATIVE, fee: 1,
        });
        let result = apply_tx(&s, &signed).unwrap();
        let transition = batch_into_transition([result], 1);
        commit_state_transition(&mut s, &transition, 1).unwrap();

        // Only the fee left the wallet; `amount` cancelled against itself. The
        // mint bug would have produced 1_500 here (start + amount). The fee is
        // burned (debited, credited to no one) — same as a normal Send — so the
        // supply DROPS by the fee, and crucially never GROWS.
        assert_eq!(s.balance_of(&alice, &NATIVE), 1_000 - 1, "self-send must only cost the fee, not mint `amount`");
        assert_eq!(s.native_supply(), supply_before - 1, "self-send burns the fee and mints nothing");
        assert!(s.native_supply() <= supply_before, "native supply must never grow on a self-send");
    }

    #[test]
    fn self_send_token_does_not_mint() {
        // Same, for a non-native token: token slot nets to zero, fee debits NATIVE.
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let tok: TokenId = [7u8; 32];
        fund(&mut s, alice, 1_000); // native for the fee
        // seed a token balance
        let seed = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance { wallet: alice, token: tok, amount: 800 }],
        };
        commit_state_transition(&mut s, &seed, 0).unwrap();

        let signed = dummy_signed(SigilTx::Send {
            from: alice, to: alice, amount: 300, token: tok, fee: 2,
        });
        let result = apply_tx(&s, &signed).unwrap();
        let transition = batch_into_transition([result], 1);
        commit_state_transition(&mut s, &transition, 1).unwrap();

        assert_eq!(s.balance_of(&alice, &tok), 800, "token self-send must leave token balance unchanged");
        assert_eq!(s.balance_of(&alice, &NATIVE), 1_000 - 2, "only the native fee is spent");
    }

    #[test]
    fn insufficient_balance_rejects() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let bob:   WalletId = [2u8; 32];
        fund(&mut s, alice, 50);
        let signed = dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 100, token: NATIVE, fee: 1,
        });
        let err = apply_tx(&s, &signed).unwrap_err();
        assert!(matches!(err, TxApplyError::InsufficientBalance { .. }));
    }

    #[test]
    fn signer_must_be_fee_payer() {
        let alice: WalletId = [1u8; 32];
        let bob:   WalletId = [2u8; 32];
        let mut signed = dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 1, token: NATIVE, fee: 1,
        });
        signed.from_pubkey = bob; // pretend bob signed alice's tx
        let err = signed.precheck().unwrap_err();
        assert!(matches!(err, TxApplyError::SignerNotPayer));
    }

    #[test]
    fn sig_length_must_match_scheme() {
        let alice: WalletId = [1u8; 32];
        let bob:   WalletId = [2u8; 32];
        let mut signed = dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 1, token: NATIVE, fee: 1,
        });
        signed.sig = SignatureBytes(vec![0u8; 64]);
        let err = signed.precheck().unwrap_err();
        assert!(matches!(err, TxApplyError::SigLengthMismatch { .. }));
    }

    #[test]
    fn unsigned_dummy_tx_is_rejected_by_real_verify() {
        // Tier-1: verify_signature is now REAL (not a NotImplemented stub).
        // A dummy tx with an empty pubkey + zero sig must be rejected — here
        // by the pubkey-length gate, before any curve op.
        let alice: WalletId = [1u8; 32];
        let bob:   WalletId = [2u8; 32];
        let signed = dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 1, token: NATIVE, fee: 1,
        });
        assert!(matches!(
            signed.verify_signature(),
            Err(TxApplyError::PubKeyLengthMismatch { .. })
        ));
    }

    #[test]
    fn swap_against_seeded_pool() {
        // P5: pool now carries token_a/token_b/fee_bps. Seed 100k:100k so the
        // 100-unit swap doesn't approach MIN_RESERVE (1000). Alice swaps 100
        // NATIVE → token_b.
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let pool_id: PoolId = [9u8; 32];
        let other_token: TokenId = [7u8; 32];
        fund(&mut s, alice, 10_000);
        let seed = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetPool {
                pool: pool_id,
                state: PoolState {
                    token_a: NATIVE,
                    token_b: other_token,
                    reserve_a: 100_000,
                    reserve_b: 100_000,
                    lp_shares: 100_000,
                    fee_bps: 30,
                    accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(&mut s, &seed, 0).unwrap();

        let signed = dummy_signed(SigilTx::Swap {
            from: alice, pool: pool_id,
            in_token: NATIVE, in_amt: 100, min_out: 80, fee: 1,
        });
        let result = apply_tx(&s, &signed).unwrap();
        assert!(matches!(result.events[0], SigilEvent::SwapExecuted { .. }));
    }

    // ── PRIVACY-ONLY GATE ───────────────────────────────────────────────────────────

    /// SIGIL is privacy-only: a transparent peer-to-peer send is refused by the CONSENSUS
    /// entry point. This is the whole point of the change — paying someone in the clear is
    /// no longer a thing the chain will do.
    #[test]
    fn transparent_send_is_refused_at_the_consensus_entry_point() {
        let (alice, bob) = ([1u8; 32], [2u8; 32]);
        let mut s = SigilState::default();
        commit_state_transition(
            &mut s,
            &StateTransition {
                at_height: 1,
                mutations: vec![StateMutation::SetBalance {
                    wallet: alice, token: NATIVE, amount: 1_000,
                }],
            },
            1,
        )
        .unwrap();

        let signed = dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 100, token: NATIVE, fee: 1,
        });
        let err = apply_tx_at(&s, &signed, SHIELDED_ONLY_HEIGHT)
            .expect_err("a transparent send must be refused at/after activation");
        assert!(
            matches!(err, TxApplyError::TransparentSendRetired { .. }),
            "got {err:?}"
        );
        // and nothing moved
        assert_eq!(s.balance_of(&alice, &NATIVE), 1_000);
        assert_eq!(s.balance_of(&bob, &NATIVE), 0);
    }

    /// The RAMPS stay open. A pool nobody can enter or leave is not privacy, it is a trap —
    /// so Shield and Unshield must survive the gate that retires transparent sends.
    #[test]
    fn shield_and_unshield_survive_the_gate() {
        let alice = [1u8; 32];
        let mut s = SigilState::default();
        commit_state_transition(
            &mut s,
            &StateTransition {
                at_height: 1,
                mutations: vec![StateMutation::SetBalance {
                    wallet: alice, token: NATIVE, amount: 1_000,
                }],
            },
            1,
        )
        .unwrap();

        let shield = dummy_signed(SigilTx::Shield {
            from: alice, amount: 500, cm: [7u8; 32], fee: 0,
        });
        assert!(
            apply_tx_at(&s, &shield, SHIELDED_ONLY_HEIGHT + 10_000).is_ok(),
            "Shield is the on-ramp and must remain available"
        );
    }

    /// Historical replay is unaffected: blocks that settled with transparent sends must
    /// still validate exactly as they did, or a node cannot replay the chain from genesis
    /// with one binary. The ungated entry is what preserves that.
    #[test]
    fn historical_replay_still_applies_transparent_sends() {
        let (alice, bob) = ([1u8; 32], [2u8; 32]);
        let mut s = SigilState::default();
        commit_state_transition(
            &mut s,
            &StateTransition {
                at_height: 1,
                mutations: vec![StateMutation::SetBalance {
                    wallet: alice, token: NATIVE, amount: 1_000,
                }],
            },
            1,
        )
        .unwrap();
        let signed = dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 100, token: NATIVE, fee: 1,
        });
        assert!(
            apply_tx(&s, &signed).is_ok(),
            "replay of an already-settled transparent send must still validate"
        );
    }

    #[test]
    fn swap_credits_master_wallet_at_the_directive_rate() {
        // Install master wallet at genesis. Run a swap large enough that
        // amount_out * 5 / 10_000 rounds to a non-zero master share. Verify
        // the master receives that share, the user receives the rest, and
        // the totals conserve.
        let mut s = SigilState::new();
        let master: WalletId = [99u8; 32];
        let alice:  WalletId = [1u8; 32];
        let pool_id: PoolId = [9u8; 32];
        let other_token: TokenId = [7u8; 32];
        fund(&mut s, alice, 10_001); // 10_000 swap + 1 fee, all native

        // Genesis-flavoured one-shot SetMasterWallet + pool seed in one
        // transition (height 0 — block 0 is the canonical install point).
        let genesis = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetMasterWallet { wallet: master },
                StateMutation::SetPool {
                    pool: pool_id,
                    state: PoolState {
                        token_a: NATIVE, token_b: other_token,
                        reserve_a: 1_000_000, reserve_b: 1_000_000,
                        lp_shares: 1_000_000, fee_bps: 30, accrued_fees: 0,
                    },
                },
            ],
        };
        commit_state_transition(&mut s, &genesis, 0).unwrap();
        assert_eq!(s.master_wallet(), Some(master));

        // Alice swaps 10_000 NATIVE for other_token. Expected:
        //   amount_in_with_fee = 10_000 * 9970 = 99_700_000
        //   num = 99_700_000 * 1_000_000  ≈ 9.97e13
        //   den = 1_000_000 * 10_000 + 99_700_000 = 10_099_700_000
        //   amount_out = floor(9.97e13 / 1.00997e10) ≈ 9871
        //   master_share = 9871 * 5 / 10_000 = 4 (floor)
        //   user_share = 9867
        let signed = dummy_signed(SigilTx::Swap {
            from: alice, pool: pool_id,
            in_token: NATIVE, in_amt: 10_000, min_out: 9_000, fee: 1,
        });
        let result = apply_tx(&s, &signed).unwrap();
        let transition = batch_into_transition([result], 1);
        commit_state_transition(&mut s, &transition, 1).unwrap();

        let alice_out = s.balance_of(&alice, &other_token);
        let master_out = s.balance_of(&master, &other_token);
        assert!(master_out > 0, "master must receive a non-zero slice on this swap size");
        assert_eq!(
            alice_out + master_out,
            // The total credited equals the AMM's amount_out for the swap.
            // Recompute it deterministically here so the test pins the math.
            {
                let amount_in_with_fee = 10_000u128 * 9970;
                let num = amount_in_with_fee * 1_000_000u128;
                let den = 1_000_000u128 * 10_000 + amount_in_with_fee;
                num / den
            },
            "alice + master must equal AMM amount_out (no leak)"
        );
        // 30 bps = 0.3% ≈ 1/333 of the output, per Viktor's 2026-06-09 directive
        // (pinned by sigil_bank::tests::rate_constants_match_user_directive).
        // This bound previously read 1/2000 (5 bps) and had been failing against
        // correct code since the rate was set — the test was stale, not the constant.
        let total_out = alice_out + master_out;
        let expected = total_out * sigil_bank::MASTER_SWAP_FEE_BPS / sigil_bank::BPS_DENOMINATOR;
        assert!(
            master_out <= expected + 1,
            "master share {master_out} exceeds {} bps of {total_out}",
            sigil_bank::MASTER_SWAP_FEE_BPS
        );
    }

    #[test]
    fn master_wallet_cannot_be_reset() {
        // Genesis installs master = wallet_A. Any later SetMasterWallet (even
        // attempting to re-install the SAME wallet) must reject. This is the
        // one-shot rule from project_sigil_chain memory + Lock #14.
        let mut s = SigilState::new();
        let m1: WalletId = [99u8; 32];
        let m2: WalletId = [100u8; 32];
        let t0 = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetMasterWallet { wallet: m1 }],
        };
        commit_state_transition(&mut s, &t0, 0).unwrap();
        assert_eq!(s.master_wallet(), Some(m1));

        let t1 = StateTransition {
            at_height: 1,
            mutations: vec![StateMutation::SetMasterWallet { wallet: m2 }],
        };
        let err = commit_state_transition(&mut s, &t1, 1).unwrap_err();
        assert!(matches!(err, sigil_state::CommitError::MasterWalletAlreadySet));
        // m1 still in place.
        assert_eq!(s.master_wallet(), Some(m1));
    }

    #[test]
    fn swap_invalid_in_token_rejected() {
        // Pool is (NATIVE, other_token). Caller passes a third token as
        // in_token — must reject loudly.
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let pool_id: PoolId = [9u8; 32];
        let other_token: TokenId = [7u8; 32];
        let bogus_token: TokenId = [42u8; 32];
        fund(&mut s, alice, 10_000);
        let seed = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetPool {
                pool: pool_id,
                state: PoolState {
                    token_a: NATIVE, token_b: other_token,
                    reserve_a: 1_000, reserve_b: 1_000,
                    lp_shares: 1_000, fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(&mut s, &seed, 0).unwrap();

        let signed = dummy_signed(SigilTx::Swap {
            from: alice, pool: pool_id,
            in_token: bogus_token, in_amt: 100, min_out: 0, fee: 1,
        });
        assert!(matches!(apply_tx(&s, &signed), Err(TxApplyError::InvalidSwapToken)));
    }

    #[test]
    fn lp_deposit_creates_pool_on_first_call() {
        // First LpDeposit defines (token_a, token_b, fee_bps). Alice has
        // 1000 NATIVE + 1000 of other_token + 1 for fee.
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let other_token: TokenId = [7u8; 32];
        let pool_id: PoolId = [9u8; 32];
        fund(&mut s, alice, 1_001);
        // Give alice some other_token directly.
        let seed = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance {
                wallet: alice, token: other_token, amount: 1_000,
            }],
        };
        commit_state_transition(&mut s, &seed, 0).unwrap();

        let signed = dummy_signed(SigilTx::LpDeposit {
            from: alice, pool: pool_id,
            token_a: NATIVE, token_b: other_token,
            amt_a: 1_000, amt_b: 1_000, fee_bps: 30, fee: 1,
        });
        let result = apply_tx(&s, &signed).unwrap();
        // Apply + verify the pool now exists with the right shape.
        let t = batch_into_transition([result], 1);
        commit_state_transition(&mut s, &t, 1).unwrap();
        let pool = s.pool(&pool_id).expect("pool created");
        assert_eq!(pool.token_a, NATIVE);
        assert_eq!(pool.token_b, other_token);
        assert_eq!(pool.reserve_a, 1_000);
        assert_eq!(pool.reserve_b, 1_000);
        assert_eq!(pool.fee_bps, 30);
        assert_eq!(pool.lp_shares, 1_000);
        assert_eq!(s.balance_of(&alice, &NATIVE), 0);
        assert_eq!(s.balance_of(&alice, &other_token), 0);
    }

    /// Amount of each side alice puts into the test pool. Comfortably above
    /// `sigil_dex::MIN_RESERVE` (1_000) so a PARTIAL withdraw still clears the
    /// DEX-004 reserve floor — a full drain is floor-exempt, a partial one is
    /// not, so a 1_000-unit pool cannot exercise the partial-withdraw path.
    const LP_SEED: u128 = 10_000;

    /// Seed a pool funded ENTIRELY by `alice`, and return its id + the shares
    /// she was credited. Shared by the LP-ownership tests below.
    fn seed_pool_funded_by_alice(
        s: &mut SigilState,
        alice: WalletId,
        other_token: TokenId,
    ) -> (PoolId, u128) {
        let pool_id: PoolId = [9u8; 32];
        fund(s, alice, LP_SEED + 1); // + the deposit fee
        let seed = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetBalance {
                wallet: alice, token: other_token, amount: LP_SEED,
            }],
        };
        commit_state_transition(s, &seed, 0).unwrap();

        let signed = dummy_signed(SigilTx::LpDeposit {
            from: alice, pool: pool_id,
            token_a: NATIVE, token_b: other_token,
            amt_a: LP_SEED, amt_b: LP_SEED, fee_bps: 30, fee: 1,
        });
        let result = apply_tx(s, &signed).unwrap();
        let t = batch_into_transition([result], 1);
        commit_state_transition(s, &t, 1).unwrap();
        let shares = s.balance_of(&alice, &lp_token_id(&pool_id));
        (pool_id, shares)
    }

    /// THE DRAIN, as a unit regression. Mirrors examples/lp_ownership_poc.rs
    /// (which uses real ed25519 keys) so the fix is covered by `fluxc test`
    /// and not only by running the example by hand.
    ///
    /// Before the per-wallet LP ledger, `LpWithdraw` checked only that the
    /// caller could pay the flat fee and then burned straight out of the pool
    /// TOTAL — so mallory, who never deposited, drained the whole pool.
    #[test]
    fn lp_withdraw_by_non_depositor_is_rejected() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let mallory: WalletId = [2u8; 32];
        let other_token: TokenId = [7u8; 32];
        let (pool_id, alice_shares) = seed_pool_funded_by_alice(&mut s, alice, other_token);
        assert!(alice_shares > 0, "alice must hold the LP shares she funded");

        // Mallory can pay the fee — that used to be the ONLY requirement.
        fund(&mut s, mallory, 1_000);
        assert_eq!(s.balance_of(&mallory, &lp_token_id(&pool_id)), 0);

        let pool_before = s.pool(&pool_id).expect("pool").clone();
        let steal = dummy_signed(SigilTx::LpWithdraw {
            from: mallory, pool: pool_id, shares: pool_before.lp_shares, fee: 10,
        });
        assert!(matches!(
            apply_tx(&s, &steal),
            Err(TxApplyError::InsufficientLpShares { have: 0, .. })
        ), "a wallet that never deposited must not be able to burn pool shares");

        // And nothing moved.
        let pool_after = s.pool(&pool_id).expect("pool");
        assert_eq!(pool_after.reserve_a, pool_before.reserve_a);
        assert_eq!(pool_after.reserve_b, pool_before.reserve_b);
        assert_eq!(pool_after.lp_shares, pool_before.lp_shares);
        assert_eq!(s.balance_of(&mallory, &other_token), 0);
    }

    /// The other half of the gate: the fix must not strand a REAL provider.
    /// Alice funded the pool, so she can withdraw and gets her reserves back,
    /// and her LP ledger zeroes out.
    #[test]
    fn lp_withdraw_by_real_depositor_still_works() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let other_token: TokenId = [7u8; 32];
        let (pool_id, alice_shares) = seed_pool_funded_by_alice(&mut s, alice, other_token);

        // She spent everything funding the pool; give her the withdraw fee.
        fund(&mut s, alice, 10);
        let signed = dummy_signed(SigilTx::LpWithdraw {
            from: alice, pool: pool_id, shares: alice_shares, fee: 10,
        });
        let result = apply_tx(&s, &signed).expect("the real provider must be able to withdraw");
        let t = batch_into_transition([result], 2);
        commit_state_transition(&mut s, &t, 2).unwrap();

        assert_eq!(s.balance_of(&alice, &lp_token_id(&pool_id)), 0, "LP ledger debited");
        assert_eq!(s.pool(&pool_id).expect("pool").lp_shares, 0, "pool total debited");
        // Her liquidity came back (she is the only provider, so she gets it all).
        assert_eq!(s.balance_of(&alice, &other_token), LP_SEED);
        assert_eq!(s.balance_of(&alice, &NATIVE), LP_SEED);
    }

    /// Over-withdrawing by a genuine provider is rejected at exactly the
    /// boundary, and a partial withdraw leaves the remainder owned.
    #[test]
    fn lp_withdraw_is_bounded_by_owned_shares() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let other_token: TokenId = [7u8; 32];
        let (pool_id, alice_shares) = seed_pool_funded_by_alice(&mut s, alice, other_token);
        fund(&mut s, alice, 100);

        // One share more than she owns → rejected.
        let too_much = dummy_signed(SigilTx::LpWithdraw {
            from: alice, pool: pool_id, shares: alice_shares + 1, fee: 10,
        });
        assert!(matches!(
            apply_tx(&s, &too_much),
            Err(TxApplyError::InsufficientLpShares { .. })
        ));

        // Exactly half → accepted, and half remains owned.
        let half = alice_shares / 2;
        let signed = dummy_signed(SigilTx::LpWithdraw {
            from: alice, pool: pool_id, shares: half, fee: 10,
        });
        let result = apply_tx(&s, &signed).expect("partial withdraw is legitimate");
        let t = batch_into_transition([result], 2);
        commit_state_transition(&mut s, &t, 2).unwrap();
        assert_eq!(s.balance_of(&alice, &lp_token_id(&pool_id)), alice_shares - half);
        assert_eq!(s.pool(&pool_id).expect("pool").lp_shares, alice_shares - half);
    }

    /// LP share accounting must be per-POOL, not per-wallet-global: shares in
    /// pool A must not authorise a withdraw from pool B.
    #[test]
    fn lp_shares_do_not_cross_pools() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let other_token: TokenId = [7u8; 32];
        let (pool_a, alice_shares) = seed_pool_funded_by_alice(&mut s, alice, other_token);
        assert!(alice_shares > 0);

        // A second, independent pool that alice has NOT funded.
        let pool_b: PoolId = [11u8; 32];
        let seed = StateTransition {
            at_height: 2,
            mutations: vec![StateMutation::SetPool {
                pool: pool_b,
                state: PoolState {
                    token_a: NATIVE, token_b: other_token,
                    reserve_a: 5_000, reserve_b: 5_000,
                    lp_shares: 5_000, fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(&mut s, &seed, 2).unwrap();
        fund(&mut s, alice, 100);

        assert_ne!(lp_token_id(&pool_a), lp_token_id(&pool_b), "pools get distinct LP tokens");
        assert_eq!(s.balance_of(&alice, &lp_token_id(&pool_b)), 0);

        let cross = dummy_signed(SigilTx::LpWithdraw {
            from: alice, pool: pool_b, shares: alice_shares, fee: 10,
        });
        assert!(matches!(
            apply_tx(&s, &cross),
            Err(TxApplyError::InsufficientLpShares { have: 0, .. })
        ), "pool-A shares must not authorise a pool-B withdraw");
    }

    /// The LP token id must be domain-separated from NATIVE and stable.
    #[test]
    fn lp_token_id_is_domain_separated() {
        let pool: PoolId = [9u8; 32];
        assert_ne!(lp_token_id(&pool), NATIVE, "LP token must never be NATIVE");
        assert_ne!(lp_token_id(&pool), pool, "LP token must not be the pool id itself");
        assert_eq!(lp_token_id(&pool), lp_token_id(&pool), "deterministic");
        assert_ne!(lp_token_id(&pool), lp_token_id(&[8u8; 32]), "distinct per pool");
    }

    #[test]
    fn lp_deposit_mismatched_pair_rejected() {
        // Pool exists as (NATIVE, other_token). A second deposit specifying
        // (NATIVE, third_token) must reject.
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let other_token: TokenId = [7u8; 32];
        let third_token: TokenId = [8u8; 32];
        let pool_id: PoolId = [9u8; 32];
        fund(&mut s, alice, 10);
        let seed = StateTransition {
            at_height: 0,
            mutations: vec![StateMutation::SetPool {
                pool: pool_id,
                state: PoolState {
                    token_a: NATIVE, token_b: other_token,
                    reserve_a: 1_000, reserve_b: 1_000,
                    lp_shares: 1_000, fee_bps: 30, accrued_fees: 0,
                },
            }],
        };
        commit_state_transition(&mut s, &seed, 0).unwrap();

        let signed = dummy_signed(SigilTx::LpDeposit {
            from: alice, pool: pool_id,
            token_a: NATIVE, token_b: third_token,
            amt_a: 10, amt_b: 10, fee_bps: 30, fee: 1,
        });
        assert!(matches!(apply_tx(&s, &signed), Err(TxApplyError::PoolMismatch)));
    }

    #[test]
    fn batch_combines_n_txs() {
        let mut s = SigilState::new();
        let alice: WalletId = [1u8; 32];
        let bob:   WalletId = [2u8; 32];
        fund(&mut s, alice, 1_000);

        let r1 = apply_tx(&s, &dummy_signed(SigilTx::Send {
            from: alice, to: bob, amount: 10, token: NATIVE, fee: 1,
        })).unwrap();
        // Note: r2's apply_tx sees the SAME pre-state as r1 because we haven't
        // committed yet. In a real mempool this would re-read after each
        // commit; this test just shows the batch shape is correct.
        let transition = batch_into_transition([r1], 1);
        assert!(transition.mutations.len() >= 3); // sender, recipient, 2 events
        commit_state_transition(&mut s, &transition, 1).unwrap();
        assert_eq!(s.balance_of(&bob, &NATIVE), 10);
    }

}


#[cfg(test)]
mod r0_nonce_binding_tests {
    use super::*;

    #[test]
    fn nonce_is_bound_to_the_signature() {
        let (sk, pk, author) = ed25519_keygen();
        let ops = vec![SigilTx::Send { from: author, to: [9u8; 32], amount: 1, token: NATIVE, fee: 0 }];
        let batch = AuthorizedBatch::sign_ed25519(ops, 5, &sk, &pk);
        batch.verify().expect("valid nonce-5 batch verifies");

        // Rebroadcast with a bumped nonce but the OLD signature -> rejected (the hole).
        let mut tampered = batch.clone();
        tampered.nonce = 6;
        assert!(matches!(tampered.verify(), Err(TxApplyError::SignatureInvalid)));

        // A freshly-signed different nonce yields a DIFFERENT signature (distinct message).
        let ops2 = vec![SigilTx::Send { from: author, to: [9u8; 32], amount: 1, token: NATIVE, fee: 0 }];
        let b6 = AuthorizedBatch::sign_ed25519(ops2, 6, &sk, &pk);
        assert_ne!(batch.sig, b6.sig, "the nonce must change the signed message");
        b6.verify().expect("nonce-6 batch verifies");
    }
}


#[cfg(test)]
mod r1_mempool_tests {
    use super::*;

    #[test]
    fn batch_lane_ingest_dedup_and_pull() {
        let (sk, pk, author) = ed25519_keygen();
        let ops = vec![SigilTx::Send { from: author, to: [9u8; 32], amount: 1, token: NATIVE, fee: 0 }];
        let b = AuthorizedBatch::sign_ed25519(ops, 1, &sk, &pk);
        let mut mp = Mempool::new();
        assert_eq!(mp.ingest_batch(b.clone()).unwrap(), 1);
        assert_eq!(mp.batch_count(), 1);
        assert_eq!(mp.pending_batch_ops(), 1);
        // dedup: the identical batch is rejected (not re-queued).
        assert!(matches!(mp.ingest_batch(b.clone()), Err(TxApplyError::DuplicateBatch)));
        assert_eq!(mp.batch_count(), 1);
        // a batch with a tampered nonce fails verify() (R0 binding) — rejected.
        let mut bad = b.clone();
        bad.nonce = 999;
        assert!(mp.ingest_batch(bad).is_err());
        // pull takes the whole batch; lane drains.
        let pulled = mp.pull_batches(10);
        assert_eq!(pulled.len(), 1);
        assert_eq!(mp.batch_count(), 0);
    }

    #[test]
    fn pull_respects_op_budget_without_splitting() {
        let (sk, pk, author) = ed25519_keygen();
        let mut mp = Mempool::new();
        for nonce in 1..=5u64 {
            let ops: Vec<SigilTx> = (0..4).map(|j| SigilTx::Send { from: author, to: [j as u8; 32], amount: nonce as u128, token: NATIVE, fee: 0 }).collect();
            mp.ingest_batch(AuthorizedBatch::sign_ed25519(ops, nonce, &sk, &pk)).unwrap();
        }
        // budget 6 ops: takes 2 full 4-op batches (>= budget after the 2nd), never a partial.
        let pulled = mp.pull_batches(6);
        assert_eq!(pulled.len(), 2);
        assert!(pulled.iter().all(|b| b.ops.len() == 4));
        assert_eq!(mp.batch_count(), 3);
    }
}


#[cfg(test)]
mod r2_apply_tests {
    use super::*;
    use sigil_header::SQISIGN_L5_LEN;

    /// Local to this module: `crate::tests::dummy_signed` is private to that module.
    /// `apply_tx*` only prechecks, so an unsigned shell is enough for gate tests.
    fn gate_signed(tx: SigilTx) -> SignedTx {
        let from = tx.fee_payer();
        SignedTx {
            tx,
            from_pubkey: from,
            nonce: 0,
            sig_scheme: SigScheme::SqiSign5,
            sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
            pubkey: PubKeyBytes(Vec::new()),
        }
    }

    /// THE MULTI-INPUT GATE, from both sides.
    ///
    /// Below the activation height a spend declaring a second input must be REFUSED, not
    /// have the extra tag quietly ignored. Ignoring it is the dangerous outcome: the proof
    /// would still be a 2-input proof, so the chain would verify a spend of two notes while
    /// recording one nullifier — leaving the other note spendable. A double-spend by
    /// omission is exactly what this gate exists to make unreachable.
    #[test]
    fn a_second_input_is_refused_below_the_activation_height() {
        let tx = SigilTx::ShieldedSend {
            anchor: [1u8; 32],
            nullifier: [2u8; 32],
            extra_nullifiers: vec![[3u8; 32]],
            cm_outs: vec![[4u8; 32], [5u8; 32]],
            fee: sigil_state::shielded::SHIELDED_FEE,
            proof: vec![0u8; 8],
            note_ciphertexts: vec![],
        };
        let signed = gate_signed(tx);
        let st = SigilState::new();

        // PRE-ACTIVATION HALF — vacuous on a chain whose gate is 0, and asserting it would
        // mean asserting something about height `0 - 1`. Guarded rather than deleted: on a
        // chain that activates this at a real height, this is the assertion that matters,
        // and rediscovering it later is how a pre-activation regime ends up untested.
        if SHIELDED_MULTI_INPUT_HEIGHT > 0 {
            let below = apply_tx_at(&st, &signed, SHIELDED_MULTI_INPUT_HEIGHT - 1);
            assert!(
                matches!(below, Err(TxApplyError::ShieldedRejected(m)) if m.contains("not active")),
                "a second input must be refused before activation, got {below:?}"
            );
        }

        // At the activation height the gate lets it through to the real checks — which
        // reject it for a different reason (the proof is nonsense here). The point is that
        // it is no longer THIS reason.
        let at = apply_tx_at(&st, &signed, SHIELDED_MULTI_INPUT_HEIGHT);
        assert!(
            !matches!(&at, Err(TxApplyError::ShieldedRejected(m)) if m.contains("not active")),
            "the gate must be open at the activation height, got {at:?}"
        );
    }

    /// The same note declared as both inputs is refused BEFORE any proof work.
    ///
    /// Load-bearing, not hygiene: the circuit ACCEPTS this witness. Both input blocks are
    /// independently valid, the conservation lane sums twice the value, and every
    /// constraint holds. The only tell is the repeated tag — and the chain stores tags in a
    /// set, so the duplicate insert is a no-op. One note burned, double the value out.
    #[test]
    fn the_same_nullifier_declared_twice_is_refused() {
        let nf = [2u8; 32];
        let tx = SigilTx::ShieldedSend {
            anchor: [1u8; 32],
            nullifier: nf,
            extra_nullifiers: vec![nf],
            cm_outs: vec![[4u8; 32], [5u8; 32]],
            fee: sigil_state::shielded::SHIELDED_FEE,
            proof: vec![0u8; 8],
            note_ciphertexts: vec![],
        };
        let signed = gate_signed(tx);
        let st = SigilState::new();
        let r = apply_tx_at(&st, &signed, SHIELDED_MULTI_INPUT_HEIGHT);
        assert!(
            matches!(r, Err(TxApplyError::ShieldedRejected(m)) if m.contains("same nullifier")),
            "one note used as both inputs must be refused, got {r:?}"
        );
    }

    /// Nothing may read only the first tag. `shielded_send_nullifiers` is the sanctioned
    /// accessor precisely because `nullifier` alone still compiles and still looks complete.
    #[test]
    fn the_accessor_returns_every_nullifier() {
        let tx = SigilTx::ShieldedSend {
            anchor: [1u8; 32],
            nullifier: [2u8; 32],
            extra_nullifiers: vec![[3u8; 32]],
            cm_outs: vec![[4u8; 32], [5u8; 32]],
            fee: 1,
            proof: vec![],
            note_ciphertexts: vec![],
        };
        assert_eq!(tx.shielded_send_nullifiers(), Some(vec![[2u8; 32], [3u8; 32]]));
        assert_eq!(
            SigilTx::Shield { from: [0u8; 32], amount: 1, cm: [0u8; 32], fee: 0 }
                .shielded_send_nullifiers(),
            None,
            "only a shielded spend has nullifiers"
        );
    }

    /// A ShieldedSend serialised BEFORE this field existed must still decode — every one
    /// already written into the chain log is that shape, and a chain that cannot replay its
    /// own history is not a chain.
    #[test]
    fn a_pre_multi_input_transaction_still_deserialises() {
        // Build the legacy shape the honest way: serialise a real transaction with the
        // real serialiser, then DELETE the field, which is exactly what a record written
        // before the field existed looks like. Hand-writing the JSON would only test that
        // my idea of the old shape round-trips.
        let modern = SigilTx::ShieldedSend {
            anchor: [1u8; 32],
            nullifier: [2u8; 32],
            extra_nullifiers: vec![],
            cm_outs: vec![[4u8; 32], [5u8; 32]],
            fee: 7,
            proof: vec![],
            note_ciphertexts: vec![],
        };
        // `SigilTx` is `#[serde(tag = "kind")]` — INTERNALLY tagged, so the JSON is flat
        // (`{"kind": "ShieldedSend", "anchor": ..., ...}`) and not nested under a variant
        // key. Getting that wrong is how this test failed first time round.
        let mut v = serde_json::to_value(&modern).expect("serialise");
        let body = v.as_object_mut().expect("a flat object");
        assert_eq!(body.get("kind").and_then(|k| k.as_str()), Some("ShieldedSend"));
        assert!(
            body.remove("extra_nullifiers").is_some(),
            "the field must have been there to remove"
        );

        let legacy: SigilTx = serde_json::from_value(v).expect("legacy shape must decode");
        assert_eq!(
            legacy.shielded_send_nullifiers(),
            Some(vec![[2u8; 32]]),
            "an old transaction has exactly one input"
        );
        assert_eq!(legacy, modern, "and is otherwise identical to the modern one-input form");
    }

    use super::*;

    #[test]
    fn apply_batch_changes_state_advances_nonce_atomically() {
        let mut st = sigil_state::SigilState::new();
        let (sk, pk, author) = ed25519_keygen();
        // fund the author so the Send has balance to move.
        sigil_state::commit_state_transition(
            &mut st,
            &StateTransition {
                at_height: 0,
                mutations: vec![sigil_state::StateMutation::SetBalance { wallet: author, token: NATIVE, amount: 1000 }],
            },
            0,
        ).unwrap();
        let root0 = st.roots().wallet_state_root;

        let ops = vec![SigilTx::Send { from: author, to: [9u8; 32], amount: 10, token: NATIVE, fee: 0 }];
        let batch = AuthorizedBatch::sign_ed25519(ops, 1, &sk, &pk);
        let roots = apply_authorized_batch(&mut st, &batch, 1).expect("valid batch applies");
        assert_ne!(root0, roots.wallet_state_root, "the batch must change wallet state");
        assert_eq!(st.nonce_of(&author), 1, "nonce advances to the batch nonce");

        // replay of the SAME batch: rejected atomically — state AND nonce unchanged.
        let before = st.roots().wallet_state_root;
        assert!(matches!(
            apply_authorized_batch(&mut st, &batch, 2),
            Err(sigil_state::CommitError::NonceReplay { .. })
        ));
        assert_eq!(st.roots().wallet_state_root, before, "replay must not change state");
        assert_eq!(st.nonce_of(&author), 1, "replay must not advance the nonce");
    }
}

// ── SIGIL-Nation: citizenship + welfare (consensus arms) ────────────────────
#[cfg(test)]
mod nation_welfare_tests {
    use super::*;
    use sigil_bank::welfare as wf;
    use sigil_header::SQISIGN_L5_LEN;
    use sigil_state::commit_state_transition;

    const MASTER: WalletId = [0xAA; 32];
    const ALICE: WalletId = [0x11; 32];
    const CPR: [u8; 32] = [0x42; 32];
    const H: u64 = wf::WELFARE_FROM_HEIGHT;

    fn signed(tx: SigilTx) -> SignedTx {
        let from = tx.fee_payer();
        SignedTx {
            tx,
            from_pubkey: from,
            nonce: 0,
            sig_scheme: SigScheme::SqiSign5,
            sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
            pubkey: PubKeyBytes(Vec::new()),
        }
    }

    /// Per-claim collateral at the fixture's $2.00 oracle price:
    /// ceil($1.00 × 1.05 × 1e10 / $2.00) = 5.25e9 glyphs (0.525 SIGIL).
    const LOCK_AT_2USD: u128 = 5_250_000_000;

    fn nation_state(treasury: u128) -> SigilState {
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![
            StateMutation::SetMasterWallet { wallet: MASTER },
            StateMutation::SetBalance { wallet: MASTER, token: NATIVE, amount: 1_000_000 },
            StateMutation::SetBalance { wallet: wf::WELFARE_WALLET, token: NATIVE, amount: treasury },
        ] }, 0).unwrap();
        // $2.00/SIGIL — the sUSD stipend needs a live oracle price.
        sigil_oracle::update_price(&mut s, 0, sigil_oracle::ORACLE_AUTHORITY, 200_000_000).unwrap();
        s
    }

    fn apply_commit(s: &mut SigilState, tx: SigilTx, h: u64) -> Result<(), TxApplyError> {
        let r = apply_tx_at(s, &signed(tx), h)?;
        commit_state_transition(s, &StateTransition { at_height: h, mutations: r.mutations }, h)
            .expect("commit after successful apply");
        Ok(())
    }

    #[test]
    fn attest_then_claim_full_lifecycle() {
        let mut s = nation_state(wf::WELFARE_STIPEND_GLYPHS * 3);
        apply_commit(&mut s, SigilTx::CitizenAttest { authority: MASTER, citizen: ALICE, cpr_hash: CPR, fee: 0 }, H).unwrap();
        assert_eq!(s.contract_slot(&wf::BORGER_REGISTRY, &ALICE), CPR);

        apply_commit(&mut s, SigilTx::WelfareClaim { citizen: ALICE, fee: 0 }, H + 1).unwrap();
        // sUSD payout: Alice holds exactly $1.00 of USDS, NO native SIGIL —
        // the treasury's SIGIL moved into the USDS vault as collateral.
        assert_eq!(s.balance_of(&ALICE, &sigil_usds::USDS), wf::WELFARE_STIPEND_USD_E8);
        assert_eq!(s.balance_of(&ALICE, &NATIVE), 0);
        assert_eq!(s.balance_of(&wf::WELFARE_WALLET, &NATIVE), wf::WELFARE_STIPEND_GLYPHS * 3 - LOCK_AT_2USD);
        assert_eq!(s.balance_of(&sigil_usds::VAULT, &NATIVE), LOCK_AT_2USD);
        assert_eq!(wf::decode_claim_height(&s.contract_slot(&wf::WELFARE_LEDGER, &ALICE)), H + 1);

        // Cooldown: immediate re-claim refused with the exact next height.
        let err = apply_tx_at(&s, &signed(SigilTx::WelfareClaim { citizen: ALICE, fee: 0 }), H + 2).unwrap_err();
        assert!(matches!(err, TxApplyError::WelfareCooldown { next_height } if next_height == H + 1 + wf::WELFARE_CLAIM_INTERVAL_BLOCKS));

        // After the interval the claim opens again — another $1.00.
        apply_commit(&mut s, SigilTx::WelfareClaim { citizen: ALICE, fee: 0 }, H + 1 + wf::WELFARE_CLAIM_INTERVAL_BLOCKS).unwrap();
        assert_eq!(s.balance_of(&ALICE, &sigil_usds::USDS), wf::WELFARE_STIPEND_USD_E8 * 2);
        assert_eq!(s.balance_of(&sigil_usds::VAULT, &NATIVE), LOCK_AT_2USD * 2);
    }

    #[test]
    fn nation_txs_refused_below_activation() {
        let s = nation_state(wf::WELFARE_STIPEND_GLYPHS);
        let att = signed(SigilTx::CitizenAttest { authority: MASTER, citizen: ALICE, cpr_hash: CPR, fee: 0 });
        assert!(matches!(apply_tx_at(&s, &att, H - 1), Err(TxApplyError::NationNotActive { .. })));
        let clm = signed(SigilTx::WelfareClaim { citizen: ALICE, fee: 0 });
        assert!(matches!(apply_tx_at(&s, &clm, H - 1), Err(TxApplyError::NationNotActive { .. })));
        // The legacy no-height entry point refuses too (unwrap_or(0)).
        assert!(matches!(apply_tx(&s, &att), Err(TxApplyError::NationNotActive { .. })));
    }

    #[test]
    fn attest_requires_master_and_nonzero_cpr() {
        let s = nation_state(0);
        let rogue = signed(SigilTx::CitizenAttest { authority: ALICE, citizen: ALICE, cpr_hash: CPR, fee: 0 });
        assert!(matches!(apply_tx_at(&s, &rogue, H).unwrap_err(), TxApplyError::NotNationAuthority));
        let empty = signed(SigilTx::CitizenAttest { authority: MASTER, citizen: ALICE, cpr_hash: [0u8; 32], fee: 0 });
        assert!(matches!(apply_tx_at(&s, &empty, H).unwrap_err(), TxApplyError::InvalidAttestation));
    }

    #[test]
    fn claim_requires_citizenship_and_funded_treasury() {
        let mut s = nation_state(0);
        // Not attested → NotCitizen.
        let clm = signed(SigilTx::WelfareClaim { citizen: ALICE, fee: 0 });
        assert!(matches!(apply_tx_at(&s, &clm, H).unwrap_err(), TxApplyError::NotCitizen));
        // Attested but treasury empty → refused, never mints.
        apply_commit(&mut s, SigilTx::CitizenAttest { authority: MASTER, citizen: ALICE, cpr_hash: CPR, fee: 0 }, H).unwrap();
        assert!(matches!(
            apply_tx_at(&s, &clm, H + 1).unwrap_err(),
            TxApplyError::WelfareTreasuryInsufficient { have: 0, .. }
        ));
    }

    #[test]
    fn claim_fee_burns_from_treasury_and_conserves() {
        let mut s = nation_state(wf::WELFARE_STIPEND_GLYPHS);
        apply_commit(&mut s, SigilTx::CitizenAttest { authority: MASTER, citizen: ALICE, cpr_hash: CPR, fee: 0 }, H).unwrap();
        let fee = 7u128;
        let native_before = s.balance_of(&ALICE, &NATIVE)
            + s.balance_of(&wf::WELFARE_WALLET, &NATIVE)
            + s.balance_of(&sigil_usds::VAULT, &NATIVE);
        apply_commit(&mut s, SigilTx::WelfareClaim { citizen: ALICE, fee }, H + 1).unwrap();
        // Alice gets the FULL $1.00 in USDS (the fee never touches her) and
        // needed no starting balance; the fee burns from the treasury.
        assert_eq!(s.balance_of(&ALICE, &sigil_usds::USDS), wf::WELFARE_STIPEND_USD_E8);
        assert_eq!(s.balance_of(&ALICE, &NATIVE), 0);
        assert_eq!(s.balance_of(&wf::WELFARE_WALLET, &NATIVE), wf::WELFARE_STIPEND_GLYPHS - LOCK_AT_2USD - fee);
        assert_eq!(s.balance_of(&sigil_usds::VAULT, &NATIVE), LOCK_AT_2USD);
        let native_after = s.balance_of(&ALICE, &NATIVE)
            + s.balance_of(&wf::WELFARE_WALLET, &NATIVE)
            + s.balance_of(&sigil_usds::VAULT, &NATIVE);
        assert_eq!(native_before - native_after, fee, "exactly the fee burns, nothing else leaves NATIVE");
        // A fee above the ceiling (one SIGIL) is refused outright — checked
        // on a second, cooldown-free citizen (the cooldown guard runs first).
        const BOB: WalletId = [0x33; 32];
        apply_commit(&mut s, SigilTx::CitizenAttest { authority: MASTER, citizen: BOB, cpr_hash: CPR, fee: 0 }, H + 2).unwrap();
        let big = signed(SigilTx::WelfareClaim { citizen: BOB, fee: wf::WELFARE_STIPEND_GLYPHS + 1 });
        assert!(matches!(apply_tx_at(&s, &big, H + 3).unwrap_err(), TxApplyError::InsufficientBalance { .. }));
    }

    #[test]
    fn claim_without_oracle_price_fails_closed() {
        // Build nation state WITHOUT the fixture's price push.
        let mut s = SigilState::new();
        commit_state_transition(&mut s, &StateTransition { at_height: 0, mutations: vec![
            StateMutation::SetMasterWallet { wallet: MASTER },
            StateMutation::SetBalance { wallet: MASTER, token: NATIVE, amount: 1_000_000 },
            StateMutation::SetBalance { wallet: wf::WELFARE_WALLET, token: NATIVE, amount: wf::WELFARE_STIPEND_GLYPHS },
        ] }, 0).unwrap();
        apply_commit(&mut s, SigilTx::CitizenAttest { authority: MASTER, citizen: ALICE, cpr_hash: CPR, fee: 0 }, H).unwrap();
        let clm = signed(SigilTx::WelfareClaim { citizen: ALICE, fee: 0 });
        assert!(matches!(
            apply_tx_at(&s, &clm, H + 1).unwrap_err(),
            TxApplyError::Usds(sigil_usds::UsdsError::NoPrice)
        ));
        // An OraclePush from the master unbricks it.
        apply_commit(&mut s, SigilTx::OraclePush { authority: MASTER, price_usd_e8: 200_000_000, fee: 0 }, H + 1).unwrap();
        apply_commit(&mut s, SigilTx::WelfareClaim { citizen: ALICE, fee: 0 }, H + 2).unwrap();
        assert_eq!(s.balance_of(&ALICE, &sigil_usds::USDS), wf::WELFARE_STIPEND_USD_E8);
    }

    #[test]
    fn oracle_push_requires_master_nonzero_price_and_activation() {
        let mut s = nation_state(0);
        // Rogue pusher refused.
        let rogue = signed(SigilTx::OraclePush { authority: ALICE, price_usd_e8: 100, fee: 0 });
        assert!(matches!(apply_tx_at(&s, &rogue, H).unwrap_err(), TxApplyError::NotNationAuthority));
        // Zero price refused — it would re-brick every claim.
        let zero = signed(SigilTx::OraclePush { authority: MASTER, price_usd_e8: 0, fee: 0 });
        assert!(matches!(apply_tx_at(&s, &zero, H).unwrap_err(), TxApplyError::ZeroOraclePrice));
        // Below activation refused, and the no-height entry point refuses too.
        let push = signed(SigilTx::OraclePush { authority: MASTER, price_usd_e8: 300_000_000, fee: 0 });
        assert!(matches!(apply_tx_at(&s, &push, H - 1), Err(TxApplyError::NationNotActive { .. })));
        assert!(matches!(apply_tx(&s, &push), Err(TxApplyError::NationNotActive { .. })));
        // A valid push lands and read_price sees it (byte-identical encoding).
        apply_commit(&mut s, SigilTx::OraclePush { authority: MASTER, price_usd_e8: 300_000_000, fee: 0 }, H).unwrap();
        assert_eq!(sigil_oracle::read_price(&s), 300_000_000);
    }
}
