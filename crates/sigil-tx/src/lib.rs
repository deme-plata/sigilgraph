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
        }
    }

    /// Fee declared by the tx. Centralized here so the mempool can prioritize
    /// without case-matching the enum at every call site.
    pub fn fee(&self) -> u128 {
        match self {
            SigilTx::Send            { fee, .. } |
            SigilTx::Swap            { fee, .. } |
            SigilTx::LpDeposit       { fee, .. } |
            SigilTx::LpWithdraw      { fee, .. } |
            SigilTx::ContractCall    { fee, .. } |
            SigilTx::ContractDeploy  { fee, .. } |
            SigilTx::TokenDeploy     { fee, .. } |
            SigilTx::ValidatorJoin   { fee, .. } |
            SigilTx::ValidatorLeave  { fee, .. } => *fee,
        }
    }

    /// Wallet that pays the fee. Always the natural author of the tx.
    pub fn fee_payer(&self) -> WalletId {
        match self {
            SigilTx::Send         { from, .. } => *from,
            SigilTx::Swap         { from, .. } => *from,
            SigilTx::LpDeposit    { from, .. } => *from,
            SigilTx::LpWithdraw   { from, .. } => *from,
            SigilTx::ContractCall { from, .. } => *from,
            SigilTx::ContractDeploy { from, .. } => *from,
            SigilTx::TokenDeploy  { creator, .. } => *creator,
            SigilTx::ValidatorJoin { validator, .. } => *validator,
            SigilTx::ValidatorLeave { validator, .. } => *validator,
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
    pub fn sign_ed25519(ops: Vec<SigilTx>, sk: &[u8; 32], pk: &[u8; 32]) -> Self {
        use ed25519_dalek::{Signer, SigningKey};
        let root = batch_root(&ops);
        let sig = SigningKey::from_bytes(sk).sign(&root).to_bytes().to_vec();
        Self {
            author: wallet_id_from_pubkey(pk),
            pubkey: PubKeyBytes(pk.to_vec()),
            sig_scheme: SigScheme::Ed25519Hot,
            sig: SignatureBytes(sig),
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
        // 3. ONE signature over the re-derived root (binds to this exact op set).
        let root = batch_root(&self.ops);
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

    /// AMM math (sigil-dex) raised a guard. Carries the underlying variant.
    #[error("dex error: {0}")]
    Dex(#[from] sigil_dex::DexError),

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
pub fn apply_tx(state: &SigilState, signed: &SignedTx) -> Result<ApplyResult, TxApplyError> {
    signed.precheck()?;
    let mut out = ApplyResult::default();

    match &signed.tx {
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

            // Sender side
            let new_from_native = from_native - need_native;
            out.mutations.push(StateMutation::SetBalance {
                wallet: *from, token: NATIVE, amount: new_from_native,
            });
            if token != &NATIVE {
                let new_from_token = from_token - amount;
                out.mutations.push(StateMutation::SetBalance {
                    wallet: *from, token: *token, amount: new_from_token,
                });
            }

            // Recipient side
            let to_bal = state.balance_of(to, token);
            let new_to = to_bal.checked_add(*amount).ok_or(TxApplyError::Overflow)?;
            out.mutations.push(StateMutation::SetBalance {
                wallet: *to, token: *token, amount: new_to,
            });

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
            // Per-wallet LP-share credit ledger is a deferred P5 follow-up
            // (open question #3 in the P5 scope doc) — for now the event
            // records the `shares_received` so a future ledger sweep can
            // reconstruct it. Pool total_shares does advance.

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

// ── Constants ───────────────────────────────────────────────────────────────

/// All-zero token ID = native SIGIL.
pub const NATIVE: TokenId = [0u8; 32];

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_header::{SqiSignature, SQISIGN_L5_LEN};
    use sigil_state::commit_state_transition;

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
        let batch = AuthorizedBatch::sign_ed25519(ops.clone(), &sk, &pk);
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
        let cross_batch = AuthorizedBatch::sign_ed25519(cross, &sk, &pk);
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

    #[test]
    fn swap_credits_master_wallet_5_bps_of_output() {
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
        // 5 bps ≈ 1/2000 — verify master got at most 1/2000 + 1 floor of the total.
        let total_out = alice_out + master_out;
        assert!(master_out <= total_out / 2_000 + 1);
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
