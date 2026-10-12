//! SHIELDED TRANSACTION QUEUE (PV-1 step 5, 2026-08-23).
//!
//! The wallet-facing entry point for private transfers. Same shape as [`crate::send`]: a
//! pending pool the producer drains once per candidate block, retired only when a
//! candidate lands on the settled spine — so a shielded tx riding an orphaned sibling is
//! retried rather than lost.
//!
//! # Authorization is the proof, not a signature
//!
//! [`ShieldedBridge::submit_shielded_send`] and [`submit_unshield`](ShieldedBridge::submit_unshield)
//! take NO wallet signature, and that is deliberate rather than an omission. A shielded
//! spend has no `from` — requiring one would reintroduce exactly the linkage the pool
//! exists to break. Authorization comes from the STARK, which
//! `sigil_state::commit_state_transition` verifies before any state moves.
//!
//! `submit_shield` is the exception: shielding debits a named transparent wallet, so it is
//! wallet-authenticated like an ordinary send.
//!
//! # Why submissions are proof-checked here too
//!
//! The chokepoint is the authority and re-checks everything. This layer verifies anyway,
//! for one reason: without it, anyone could flood the queue with garbage proofs that cost
//! a full STARK verification per candidate block, every block, until they aged out. A
//! cheap rejection at the door is a denial-of-service guard, never the security boundary —
//! if these two ever disagree, the chokepoint wins and this layer is the bug.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sigil_state::WalletId;
use sigil_tx::{SigilTx, SignedTx};

use crate::send::to_signed;

/// Retry budget per shielded tx.
///
/// 2026-08-24: was 60/120s, inherited from `send`'s ORIGINAL constants before this
/// chain's DAGKnight braid had a real finality window. `dag_drain_apply`
/// (`sigil-node/src/dag.rs`) only state-applies blocks once they cross
/// `BraidConfig::final_depth` (512, bumped from 64 on 2026-08-15) — measured live
/// against the real producer at ~3.5s/block, that is **~30 minutes** between a
/// candidate being proposed and it ever being eligible to settle. A tx riding on
/// fresh, not-yet-finalized candidates for only 120s gives up ~15x faster than the
/// chain could ever land it — it is not possible for ANY shielded tx to succeed
/// under the old constant, not a rare/edge failure.
///
/// Confirmed by direct live reproduction: submitted a real `RegisterShieldedAddress`
/// against the production API, watched its exact tx hash in the live log —
/// `✗ shielded tx gave up after 19 attempts / 122.3s hash=9a9fe445...` — while
/// `finalized_height` was still ~488 blocks behind where that tx's candidate was
/// proposed. This is why the shielded-mining-reward feature has never actually
/// landed a single note on the live chain since it shipped.
///
/// 40 minutes gives real margin over the ~30-minute measured floor (reorg/backlog
/// variance, not just the happy path). `MAX_ATTEMPTS` raised so it can't become the
/// new accidental limiter — the actual retry cadence is roughly one per candidate
/// mint, so 512 attempts covers the whole final_depth window with room to spare.
/// 2026-08-26: was 600, which BOUND BEFORE `MAX_AGE` and so re-broke the very thing the
/// 2026-08-24 MAX_AGE fix below was meant to repair — observed live as
/// `✗ shielded tx gave up after 600 attempts / 92.0s`. The offer cadence is one per
/// candidate mint (~6.7/s measured), so 600 offers is ~92s, well under the 2_400s this
/// path is supposed to wait. Raised to match every other bridge in this crate.
const MAX_ATTEMPTS: u32 = 30_000;
/// How long a shielded tx may stay pending before it is dropped.
const MAX_AGE: Duration = Duration::from_secs(2_400);

/// Does `pool` already contain the effects of `tx`? True means the tx is in a block
/// whose state `pool` reflects — on the frontier: "in flight, awaiting finality"; on the
/// settled chain: "applied". The predicate is deliberately the tx's OWN fingerprint, not
/// just its nullifier: a `ShieldedSend` counts only when every input nullifier is spent
/// AND every output commitment is held. A different spend of the same note (stale client,
/// or the index-0 same-amount collision the browser wallet had) leaves at least one of
/// those false, so it is still offered and still honestly refused by the chokepoint.
///
/// `RegisterShieldedAddress` counts as carried once the pool publishes its keys.
pub fn already_in_pool(pool: &sigil_state::shielded::ShieldedPool, tx: &SigilTx) -> bool {
    match tx {
        SigilTx::Shield { cm, .. } => pool.has_ever_held(cm),
        SigilTx::ShieldedSend { anchor, nullifier, extra_nullifiers, cm_outs, .. } => spend_fingerprint_present(
            std::iter::once(nullifier).chain(extra_nullifiers.iter()),
            cm_outs,
            |nf| spent_under_anchor(pool, anchor, nf),
            |cm| pool.has_ever_held(cm),
        ),
        SigilTx::Unshield { anchor, nullifier, cm_outs, .. } => spend_fingerprint_present(
            std::iter::once(nullifier),
            cm_outs,
            |nf| spent_under_anchor(pool, anchor, nf),
            |cm| pool.has_ever_held(cm),
        ),
        // A registration is carried once the pool publishes exactly these keys for the
        // wallet. Idempotent to re-apply, so this is about honesty, not safety: without it
        // `/v1/transactions/:hash` never said "in flight" for a registration, and the
        // address lookup below could not tell "unregistered" from "publishing right now".
        SigilTx::RegisterShieldedAddress { wallet, pk_shield, pk_encrypt, .. } => {
            pool.shielded_address(wallet) == Some(*pk_shield)
                && pk_encrypt.as_ref().map_or(true, |e| pool.encrypt_key(wallet) == Some(*e))
        }
        _ => false,
    }
}

/// Is `nf` spent in the pool generation `anchor` belongs to? The chain files a spend under
/// `scoped_nullifier(epoch_of_anchor(anchor), nf)`; `pool.is_spent(nf)` asks about epoch 0
/// only. Measured 2026-10-01 from Viktor's phone: a payment spending an EPOCH-1 note landed
/// (spent-count 2,583 → 2,584) but this predicate kept answering "not in the pool", so it was
/// never retired and was re-offered to 3,000+ candidates — before 0bf72c1c that produced the
/// false "rejected: double-spend" every phone saw; after it, an eternal "pending".
fn spent_under_anchor(pool: &sigil_state::shielded::ShieldedPool, anchor: &[u8; 32], nf: &[u8; 32]) -> bool {
    match pool.epoch_of_anchor(anchor) {
        Some(epoch) => pool.is_spent_in_epoch(epoch, nf),
        None => pool.is_spent(nf),
    }
}

/// A spend's own fingerprint: every input nullifier spent AND every output held.
fn spend_fingerprint_present<'a>(
    mut nullifiers: impl Iterator<Item = &'a [u8; 32]>,
    cm_outs: &[[u8; 32]],
    spent: impl Fn(&[u8; 32]) -> bool,
    held: impl Fn(&[u8; 32]) -> bool,
) -> bool {
    nullifiers.all(|nf| spent(nf)) && cm_outs.iter().all(|cm| held(cm))
}

struct Pending {
    tx: SigilTx,
    attempts: u32,
    first_seen: Instant,
    /// Candidates in which the builder reported a PERMANENT apply failure for this tx.
    /// On a braid one candidate's ordering is not the last word, so eviction waits for
    /// `REJECT_AFTER` independent verdicts rather than the first.
    permanent_fails: u32,
    /// True while the tx is already carried by the candidate spine the NEXT candidate is
    /// being built on — i.e. it is in a block, that block just has not crossed finality
    /// yet. Set by `snapshot_for_mint_excluding`; read by `status`.
    in_flight: bool,
}

/// How many candidates must refuse a tx for a permanent reason before it is evicted.
const REJECT_AFTER: u32 = 3;
/// How long a settled or rejected outcome stays answerable via `/v1/transactions/:hash`.
const OUTCOME_TTL: Duration = Duration::from_secs(6 * 3600);
const OUTCOME_CAP: usize = 16_384;

/// What the bridge knows about a shielded tx it has seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxOutcome {
    /// Still being offered to candidates — or, when `in_flight`, already inside a
    /// candidate on the DAG frontier and simply waiting for that block to finalize.
    Pending { attempts: u32, permanent_fails: u32, in_flight: bool },
    /// Settled at finality.
    Applied,
    /// Evicted after `REJECT_AFTER` permanent apply failures; the builder's reason.
    Rejected(String),
}

/// Why a shielded submission was refused at the door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShieldedSubmitError {
    BadHex(&'static str),
    BadLength { field: &'static str, expected: usize, got: usize },
    ZeroAmount,
    /// Not a permitted ramp denomination — see `sigil_state::shielded::DENOMINATIONS`.
    NotADenomination { amount: u128, suggestion: Option<Vec<u128>> },
    /// A shielded send must pay exactly the fixed fee.
    WrongFee { expected: u128, got: u128 },
    /// The proof did not verify against the supplied public inputs.
    ProofRejected(String),
    /// This nullifier is already queued — a duplicate submission, not a double-spend
    /// (the chokepoint owns that verdict).
    AlreadyQueued,
    /// Wrong number of output commitments for the circuit's fixed arity.
    WrongOutputCount { expected: usize, got: usize },
    /// Two inputs name the same nullifier — one note offered twice in one spend.
    ///
    /// Refused here even though the chain refuses it too, because this is the only layer a
    /// hand-built request passes through. The circuit cannot catch it: two identical input
    /// blocks are each independently valid and the conservation lane simply sums twice the
    /// value, so the proof VERIFIES. Only the repeated tag reveals it, and the chain stores
    /// tags in a set, so the duplicate insert is a no-op — one note burned, double the value
    /// out of the pool.
    DuplicateNullifier,
    /// The signature does not verify against the named wallet's own key.
    ///
    /// 2026-08-23: `Shield` and `RegisterShieldedAddress` both name a wallet whose
    /// transparent balance or future income the tx redirects — exactly the property
    /// this module's own doc comment says is "wallet-authenticated like an ordinary
    /// send." It was not; nothing here ever checked a signature, so anyone who knew a
    /// wallet's address could drain its transparent balance into a note only they
    /// control, or hijack its future mining rewards. This is the fix.
    SignatureInvalid,
    /// This wallet's nonce has already been used or superseded — replay protection,
    /// same mechanism `SendBridge` already uses.
    ReplayedNonce,
    /// `note_ciphertexts` was neither empty (no delivery attached) nor one entry per
    /// output — a length that fits neither shape means the positional alignment with
    /// `cm_outs` cannot be trusted.
    WrongCiphertextCount { expected: usize, got: usize },
    /// One `note_ciphertexts` entry is larger than any honest sealed note can be
    /// (`sigil_shield::note_cipher::MAX_NOTE_CIPHERTEXT_LEN`). Every node stores and
    /// hashes every ciphertext forever; this is the only thing stopping a sender from
    /// attaching kilobytes of junk to each output.
    CiphertextTooLong { got: usize, limit: usize },
    /// sigil-g3 (2026-10-19, chain rule): a deposit or an output without a sealed,
    /// view-tagged delivery envelope. The chain refuses such a note
    /// (`sigil_state::shielded::ShieldedError::UntaggedEnvelope`); the door says so at once.
    UntaggedEnvelope,
    /// An input nullifier is ALREADY in the chain's spent set, in the epoch the proof's
    /// anchor belongs to. Refused at the door (2026-09-25) because until then the door
    /// only consulted its own queue: a wallet that picked a spent note got `ok:true`, a
    /// signed acceptance receipt and "✓ sent", and the builder's refusal arrived ~14 s
    /// later as a reversal. Carries the RAW nullifier, as the chain's own error does.
    AlreadySpent([u8; 32]),
    /// Transparent → shielded deposits are paused by the operator
    /// (`SIGIL_SHIELD_DEPOSITS=pause`). An emergency brake; off by default since the door
    /// checks every deposit's opening (see `DepositOpeningMissing`).
    DepositsPaused,
    /// The deposit did not carry its opening (`blinding` + `pk_shield`).
    ///
    /// Why the door insists (2026-09-27/28): nothing on the deposit path used to prove that
    /// `cm` commits to `amount`. The chokepoint's `Shield` arm checks the denomination and
    /// the balance, never the note, so a deposit of the smallest denomination could carry a
    /// note worth far more, and the later spend proof — honest about the NOTE — would pay it
    /// out of the pool, bounded only by `value_locked`. The opening costs no privacy on a
    /// deposit: the amount is already public here, the key is the owner's published one,
    /// and a spend's nullifier `compress2(spend_key, position)` involves neither.
    DepositOpeningMissing,
    /// The opening does not reproduce `cm`: the note is not worth `amount` (or is not owned
    /// by `pk_shield`).
    DepositOpeningMismatch,
    /// A 64-bit-format deposit after the pool opened its 256-bit (v7) generation. Consensus
    /// refuses it too (`sigil_state::shielded::ShieldedError::V1DepositsClosed`); the door
    /// says so immediately so the wallet is not left waiting on a rejection.
    V1DepositsClosed,
}

/// Whether transparent → shielded deposits are paused by the operator. Opt-in brake:
/// `SIGIL_SHIELD_DEPOSITS=pause`. Deposits are otherwise open, each one opening-checked.
pub fn shield_deposits_paused() -> bool {
    matches!(std::env::var("SIGIL_SHIELD_DEPOSITS").as_deref(), Ok("pause"))
}

/// Recompute a deposit's leaf from its opening and require it to equal `cm`.
///
/// `cm = compress2(compress2(amount, blinding), pk_shield)` — `note_v1::Note::commitment`.
/// Every input goes through the strict `from_wire`, so a second spelling of the same field
/// element cannot slip past the comparison.
pub fn check_deposit_opening(
    amount: u128,
    cm: &[u8; 32],
    blinding_hex: Option<&str>,
    pk_hex: Option<&str>,
) -> Result<(), ShieldedSubmitError> {
    use sigil_shield::mimc::compress2;
    use sigil_shield::note_v1::{from_wire, to_wire, RANGE_BITS};
    let (Some(b), Some(pk)) = (blinding_hex, pk_hex) else {
        return Err(ShieldedSubmitError::DepositOpeningMissing);
    };
    // v7 (2026-10-12): a 256-bit commitment carries a 256-bit blinding and owner key, and
    // opens as `merge(H(DOM_VAL ‖ amount ‖ blinding), pk)`. The shape of `cm` says which
    // format the depositor used — the same rule consensus applies (`is_v1_shaped`).
    if !sigil_state::shielded::is_v1_shaped(cm) {
        let amount = u64::try_from(amount).map_err(|_| ShieldedSubmitError::DepositOpeningMismatch)?;
        let expected = sigil_shield::note_v7::commitment_from_opening_wire(amount, &hex32(b, "blinding")?, &hex32(pk, "pk_shield")?)
            .map_err(|_| ShieldedSubmitError::DepositOpeningMismatch)?;
        return if &expected == cm { Ok(()) } else { Err(ShieldedSubmitError::DepositOpeningMismatch) };
    }
    let amount = u64::try_from(amount)
        .ok()
        .filter(|a| *a < (1u64 << RANGE_BITS))
        .ok_or(ShieldedSubmitError::DepositOpeningMismatch)?;
    let mut amount_wire = [0u8; 32];
    amount_wire[..8].copy_from_slice(&amount.to_le_bytes());
    let fe = |w: &[u8; 32]| from_wire(w).map_err(|_| ShieldedSubmitError::DepositOpeningMismatch);
    let value = fe(&amount_wire)?;
    let blinding = fe(&hex32(b, "blinding")?)?;
    let owner = fe(&hex32(pk, "pk_shield")?)?;
    let expected = to_wire(compress2(compress2(value, blinding), owner));
    if &expected == cm {
        Ok(())
    } else {
        Err(ShieldedSubmitError::DepositOpeningMismatch)
    }
}

impl ShieldedSubmitError {
    pub fn message(&self) -> String {
        match self {
            Self::BadHex(f) => format!("{f} must be hex"),
            Self::BadLength { field, expected, got } => {
                format!("{field} must be {expected} bytes, got {got}")
            }
            Self::ZeroAmount => "amount must be > 0".into(),
            Self::DuplicateNullifier => {
                "two inputs name the same nullifier (one note cannot be spent twice in one \
                 transaction)"
                    .into()
            }
            Self::NotADenomination { amount, suggestion } => match suggestion {
                Some(parts) => format!(
                    "{amount} is not a standard ramp denomination (a distinctive amount can be \
                     correlated across the transparent boundary). Split it: {parts:?}"
                ),
                None => format!(
                    "{amount} is not a standard ramp denomination and cannot be expressed as \
                     a sum of them — use a multiple of {}",
                    sigil_state::shielded::DENOMINATIONS[0]
                ),
            },
            Self::WrongFee { expected, got } => format!(
                "shielded sends must pay exactly {expected} (got {got}) — a chosen fee is a \
                 fingerprint that identifies the sender"
            ),
            Self::ProofRejected(e) => format!("proof rejected: {e}"),
            Self::AlreadyQueued => "a transaction spending this note is already queued".into(),
            Self::WrongOutputCount { expected, got } => {
                format!("expected {expected} output commitments, got {got}")
            }
            Self::SignatureInvalid => {
                "signature does not verify against the named wallet's own key".into()
            }
            Self::ReplayedNonce => "nonce already used — sign with a higher nonce".into(),
            Self::WrongCiphertextCount { expected, got } => format!(
                "note_ciphertexts must be empty or have exactly {expected} entries (one per \
                 output), got {got}"
            ),
            Self::UntaggedEnvelope => "every note on this chain must carry a sealed envelope with a \
                view tag — update the wallet (nothing was queued)".into(),
            Self::CiphertextTooLong { got, limit } => format!(
                "a note ciphertext is {got} bytes; the limit is {limit} (a sealed note with a \
                 full {}-byte memo is well under it)",
                sigil_shield::note_cipher::MEMO_LEN
            ),
            Self::AlreadySpent(nf) => format!(
                "nullifier {} is already spent on chain — this note was paid out before; \
                 rescan your notes and pick an unspent one (nothing was queued)",
                hex::encode(nf)
            ),
            Self::DepositsPaused => "deposits into the private pool are paused for maintenance; \
                 your transparent balance was not touched and existing private notes can still \
                 be spent (nothing was queued)"
                .into(),
            Self::DepositOpeningMissing => "this wallet version cannot deposit into the private \
                 pool any more: the deposit must carry its note's blinding and owner key. Update \
                 the wallet (nothing was queued, your balance was not touched)"
                .into(),
            Self::DepositOpeningMismatch => "the deposit's note does not match its amount and \
                 owner key, so it was refused (nothing was queued)"
                .into(),
            Self::V1DepositsClosed => "the private pool now uses 256-bit notes; deposits in the \
                 old format are closed. Update the wallet (nothing was queued, your balance was \
                 not touched)"
                .into(),
        }
    }
}

/// Verify `sig_hex` (128-hex Ed25519) over `msg`, signed by `wallet`'s own key — the
/// SAME canonical-message + verify pattern `SendBridge::submit` uses, so a wallet that
/// already knows how to sign a send can sign these with the same primitive.
fn verify_wallet_sig(
    wallet: &[u8; 32],
    msg: &str,
    sig_hex: &str,
) -> Result<(), ShieldedSubmitError> {
    let sig_hex_trimmed = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    let sig_bytes = hex::decode(sig_hex_trimmed).map_err(|_| ShieldedSubmitError::SignatureInvalid)?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| ShieldedSubmitError::SignatureInvalid)?;
    let vk = VerifyingKey::from_bytes(wallet).map_err(|_| ShieldedSubmitError::SignatureInvalid)?;
    let sig = Signature::from_bytes(&sig_arr);
    vk.verify(msg.as_bytes(), &sig).map_err(|_| ShieldedSubmitError::SignatureInvalid)
}

/// How to move `amount` into (or out of) the pool: the exact denominations to use.
///
/// Exhaustive by construction — the ladder includes 1, so every integer amount decomposes
/// with no stranded remainder. A wallet calls this, derives one note per part, and submits
/// them together.
pub fn plan_shield(amount: u128) -> Result<Vec<u128>, ShieldedSubmitError> {
    sigil_state::shielded::decompose(amount)
        .ok_or(ShieldedSubmitError::NotADenomination { amount, suggestion: None })
}

/// Reject a ramp amount that is not a standard denomination, suggesting a split.
///
/// The chokepoint enforces this too; refusing here means the caller gets an actionable
/// error instead of a transaction that vanishes at mint.
fn check_denomination(amount: u128) -> Result<(), ShieldedSubmitError> {
    if sigil_state::shielded::is_denomination(amount) {
        return Ok(());
    }
    Err(ShieldedSubmitError::NotADenomination {
        amount,
        suggestion: sigil_state::shielded::decompose(amount),
    })
}

/// Decode a 32-byte hex field.
fn hex32(s: &str, field: &'static str) -> Result<[u8; 32], ShieldedSubmitError> {
    let v = hex::decode(s.trim_start_matches("0x"))
        .map_err(|_| ShieldedSubmitError::BadHex(field))?;
    if v.len() != 32 {
        return Err(ShieldedSubmitError::BadLength { field, expected: 32, got: v.len() });
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Door check against the chain's spent set: refuse a spend whose input nullifier is
/// already spent in the epoch its anchor belongs to — the exact test
/// `ShieldedPool::spend_nullifier_in_epoch` applies at commit, run before anything is
/// queued or a receipt is signed. An anchor the pool does not know is NOT refused here:
/// the chain owns that verdict, and this check must never refuse what the chain accepts.
/// Pure over the pool so it is tested without a node.
pub fn refuse_if_spent(
    pool: &sigil_state::shielded::ShieldedPool,
    anchor: &str,
    nullifiers: &[&str],
) -> Result<(), ShieldedSubmitError> {
    let anchor = hex32(anchor, "anchor")?;
    let Some(epoch) = pool.epoch_of_anchor(&anchor) else { return Ok(()) };
    for n in nullifiers {
        let nf = hex32(n, "nullifier")?;
        if pool.is_spent_in_epoch(epoch, &nf) {
            return Err(ShieldedSubmitError::AlreadySpent(nf));
        }
    }
    Ok(())
}

/// One shielded-pool operation, carrying exactly what its HTTP handler
/// received. Lets a caller (sigil-node's Dandelion relay, see
/// `dandelion_relay.rs`) hand a peer the SAME request a wallet would have
/// sent, so the peer's own `submit_*` independently re-verifies it — this
/// crate never has to know what "Dandelion" or "gossip" even are.
#[derive(Debug, Serialize, Deserialize)]
pub enum ShieldedOp {
    Register(RegisterRequest),
    Shield(ShieldRequest),
    ShieldedSend(ShieldedSendRequest),
    Unshield(UnshieldRequest),
}

/// The pending pool of shielded transactions.
#[derive(Default)]
pub struct ShieldedBridge {
    pending: Mutex<HashMap<[u8; 32], Pending>>,
    /// Terminal outcomes, so a wallet asking about a hash gets an answer instead of
    /// "unknown": `(outcome, when)`, pruned by age and size.
    outcomes: Mutex<HashMap<[u8; 32], (TxOutcome, Instant)>>,
    /// Nullifiers already represented in the queue, so a duplicate submission does not
    /// occupy two slots and waste two verifications per block.
    queued_nullifiers: Mutex<HashMap<[u8; 32], [u8; 32]>>,
    /// Replay protection for `Shield` and `RegisterShieldedAddress` — same mechanism
    /// `SendBridge::nonce_watermark` uses. A separate namespace from `SendBridge`'s is
    /// fine: the canonical message each verifies includes its own action name, so a
    /// signature valid for one action never verifies for another regardless of nonce
    /// bookkeeping.
    nonce_watermark: Mutex<HashMap<WalletId, u64>>,
    /// Fired on every successful submit, after the op is already queued
    /// locally — this crate's ONLY coupling point to P2P relay. `None` (the
    /// default) is a complete no-op, so every existing caller (tests, any
    /// caller that never wires this) is unaffected.
    relay_hook: Mutex<Option<Box<dyn Fn([u8; 32], ShieldedOp) + Send + Sync>>>,
    /// Where status changes are PUSHED (2026-09-12, instant confirm delivery): the
    /// same transitions `status()` reports — `accepted` at enqueue, `in_block` when a
    /// candidate on the spine carries the tx, `applied` / `rejected` from
    /// `remember()`. `None` (the default) publishes nothing, so every existing caller
    /// and test is unaffected; sigil-node wires the node's bus at startup.
    events: Mutex<Option<Arc<crate::events::EventBus>>>,
    /// What the door needs to know about the pool (2026-10-12, v7): which note generation
    /// an anchor belongs to, so the proof precheck runs the matching verifier, and whether
    /// the v7 generation is open, so an old-format deposit is turned away at once. `None`
    /// (tests, non-consensus callers) behaves exactly as before v7: everything is v1.
    pool_view: Mutex<Option<Box<dyn PoolView>>>,
}

/// The door's read-only window onto the settled pool. sigil-node hands in the shared state.
pub trait PoolView: Send + Sync {
    fn generation_of_anchor(&self, root: &[u8; 32]) -> Option<sigil_shield::note_v7::PoolGeneration>;
    fn v7_active(&self) -> bool;
}

impl PoolView for Arc<std::sync::RwLock<sigil_state::SigilState>> {
    fn generation_of_anchor(&self, root: &[u8; 32]) -> Option<sigil_shield::note_v7::PoolGeneration> {
        let g = match self.read() { Ok(g) => g, Err(p) => p.into_inner() };
        g.shielded().generation_of_anchor(root)
    }
    fn v7_active(&self) -> bool {
        let g = match self.read() { Ok(g) => g, Err(p) => p.into_inner() };
        g.shielded().v7_active()
    }
}

impl ShieldedBridge {
    /// Give the door its window onto the pool. Called once at node startup.
    pub fn set_pool_view(&self, view: impl PoolView + 'static) {
        *self.pool_view.lock().unwrap() = Some(Box::new(view));
    }

    /// The generation the proof precheck must use for `anchor`. An anchor the view does not
    /// know (or no view at all) is treated as v1 — today's behaviour — and the chain stays
    /// the authority on unknown anchors.
    fn generation_for(&self, anchor: &[u8; 32]) -> sigil_shield::note_v7::PoolGeneration {
        self.pool_view
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|v| v.generation_of_anchor(anchor))
            .unwrap_or(sigil_shield::note_v7::PoolGeneration::V1)
    }

    fn v7_active(&self) -> bool {
        self.pool_view.lock().unwrap().as_ref().is_some_and(|v| v.v7_active())
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the bus that `/v1/events`, `/v1/transactions/:hash/wait` and the webhook
    /// dispatcher all read. Additive: the bridge's own bookkeeping is unchanged.
    pub fn set_events(&self, bus: Arc<crate::events::EventBus>) {
        *self.events.lock().unwrap() = Some(bus);
    }

    fn emit(&self, hash: &[u8; 32], status: &str, reason: Option<String>) {
        // Clone the Arc out so the publish never runs under this mutex.
        let bus = self.events.lock().unwrap().clone();
        if let Some(b) = bus {
            b.tx_event(hash, status, reason);
        }
    }

    /// Wire a hook to be called `(id, op)` after every successful submit —
    /// e.g. sigil-node's Dandelion relay, to propagate the op to peers.
    /// Purely additive: the op is already queued locally before this fires,
    /// so a hook that never runs (or fails) costs nothing but relay reach.
    pub fn set_relay_hook(&self, hook: impl Fn([u8; 32], ShieldedOp) + Send + Sync + 'static) {
        *self.relay_hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn fire_relay_hook(&self, id: [u8; 32], op: ShieldedOp) {
        if let Some(hook) = self.relay_hook.lock().unwrap().as_ref() {
            hook(id, op);
        }
    }

    /// Check-and-advance this wallet's nonce watermark. Shared by every signed
    /// submission below so replay protection can't drift between them.
    fn check_nonce(&self, wallet: &WalletId, req_nonce: u64) -> Result<(), ShieldedSubmitError> {
        let mut wm = self.nonce_watermark.lock().unwrap();
        let last = wm.get(wallet).copied().unwrap_or(0);
        if req_nonce <= last {
            return Err(ShieldedSubmitError::ReplayedNonce);
        }
        wm.insert(*wallet, req_nonce);
        Ok(())
    }

    /// Queue a shielded-address registration.
    ///
    /// After this lands, block rewards for `wallet` are minted directly into the pool
    /// instead of crediting a transparent balance. This is the mechanism that grows the
    /// anonymity set without asking anyone to change what they do — a miner registers once
    /// and every subsequent reward is a pool note.
    ///
    /// Wallet-authenticated as of 2026-08-23: `sig` must verify against `wallet`'s own
    /// key over `sigil-rpc/v1|shield-register|{wallet}|{pk_shield}|{pk_encrypt}|{fee}|nonce={req_nonce}`
    /// — the same canonical-message pattern `SendBridge::submit` uses. Before this fix
    /// NOTHING checked that the caller owned `wallet`: anyone who knew any miner's
    /// address could redirect that miner's future block rewards to a shield key of
    /// their own choosing.
    ///
    /// `pk_encrypt` (added 2026-08-24, hex X25519 key) is folded into the SAME signed
    /// message rather than accepted separately: without binding it to the signature, an
    /// attacker could not redirect the shield key, but COULD still swap in their own
    /// encryption key on an otherwise-honest registration and silently intercept every
    /// future note ciphertext sealed to this wallet.
    /// PROOF OF POSSESSION for an optional SQIsign L5 key.
    ///
    /// Without this, publishing a post-quantum key is a HIJACK primitive rather than a
    /// defence: the registration itself is authorized by Ed25519 only, so the very adversary
    /// this upgrade defends against — one who can forge Ed25519 — could register THEIR
    /// SQIsign key against YOUR wallet. Ramps would then require a signature only they can
    /// produce. They gain the account and you lose it, delivered by the security feature.
    ///
    /// So the registrant must sign the binding themselves, with the key being registered.
    /// The message commits to the WALLET, so a signature harvested from some other context
    /// cannot be replayed here, and one made for wallet A cannot register key K to wallet B.
    fn verify_sqi_possession(
        wallet_hex: &str,
        pk_sqi_hex: &str,
        pop_sig_hex: &str,
    ) -> Result<Vec<u8>, ShieldedSubmitError> {
        let pk = hex::decode(pk_sqi_hex).map_err(|_| ShieldedSubmitError::SignatureInvalid)?;
        if pk.len() != sigil_state::shielded::SQI_PUBLIC_KEY_LEN {
            return Err(ShieldedSubmitError::SignatureInvalid);
        }
        let sig = hex::decode(pop_sig_hex).map_err(|_| ShieldedSubmitError::SignatureInvalid)?;
        let msg = format!("sigil-rpc/v1|shield-sqi-pop|{wallet_hex}|{pk_sqi_hex}");
        match flux_sqisign::verify(msg.as_bytes(), &sig, &pk) {
            Ok(true) => Ok(pk),
            _ => Err(ShieldedSubmitError::SignatureInvalid),
        }
    }

    pub fn submit_register(
        &self,
        wallet: &str,
        pk_shield: &str,
        pk_encrypt: &str,
        fee: u128,
        sig: &str,
        req_nonce: u64,
        pk_sqi: Option<&str>,
        sqi_pop: Option<&str>,
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        let w = hex32(wallet, "wallet")?;
        let pk = hex32(pk_shield, "pk_shield")?;
        let pk_enc = hex32(pk_encrypt, "pk_encrypt")?;
        // The SQIsign key is bound into the Ed25519-signed message too, so the two halves
        // cannot be mixed and matched: an attacker cannot take a valid Ed25519 registration
        // and swap in a different (validly self-signed) SQIsign key.
        let sqi_part = pk_sqi.map(|k| format!("|sqi={k}")).unwrap_or_default();
        let msg = format!(
            "sigil-rpc/v1|shield-register|{wallet}|{pk_shield}|{pk_encrypt}|{fee}|nonce={req_nonce}{sqi_part}"
        );
        verify_wallet_sig(&w, &msg, sig)?;
        let sqi_key: Option<Vec<u8>> = match (pk_sqi, sqi_pop) {
            (None, _) => None,
            // A key with no possession proof is refused outright rather than stored
            // unverified — storing it would be the hijack this check exists to prevent.
            (Some(_), None) => return Err(ShieldedSubmitError::SignatureInvalid),
            (Some(k), Some(pop)) => Some(Self::verify_sqi_possession(wallet, k, pop)?),
        };
        self.check_nonce(&w, req_nonce)?;
        let id = self.enqueue(
            SigilTx::RegisterShieldedAddress {
                wallet: w, pk_shield: pk, pk_encrypt: Some(pk_enc), fee, pk_sqi: sqi_key,
            },
            None,
        );
        self.fire_relay_hook(id, ShieldedOp::Register(RegisterRequest {
            wallet: wallet.to_string(), pk_shield: pk_shield.to_string(),
            pk_encrypt: pk_encrypt.to_string(), fee, sig: sig.to_string(), req_nonce,
            // Relayed verbatim so a relaying node re-runs the SAME proof-of-possession
            // check this node just ran, rather than trusting that we did.
            pk_sqi: pk_sqi.map(|s| s.to_string()),
            sqi_pop: sqi_pop.map(|s| s.to_string()),
        }));
        Ok(id)
    }

    /// Queue a transparent → shielded deposit.
    ///
    /// Wallet-authenticated as of 2026-08-23: `sig` must verify against `from`'s own key
    /// over `sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={req_nonce}`. Before this
    /// fix this function's own doc comment claimed the signature was "checked the same way
    /// an ordinary send's is, by the producer's apply path" — it was not; `apply_tx`'s
    /// `Shield` arm only ever checked `from` had a sufficient balance, never that the
    /// caller owned `from`. Anyone who knew any wallet's address could drain its
    /// transparent balance into a note only they control. This is the actual fix that
    /// comment was describing.
    pub fn submit_shield(
        &self,
        from: &str,
        amount: u128,
        cm: &str,
        fee: u128,
        sig: &str,
        req_nonce: u64,
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        self.submit_shield_with_delivery(from, amount, cm, fee, sig, req_nonce, None)
    }

    /// `submit_shield` with the note sealed to the depositor's OWN delivery key attached
    /// (2026-09-08). Any client of the seed then finds the deposit by trial-decryption,
    /// the way it finds a received payment — no index guessing, no client-local journal.
    /// Not signed into the wallet signature (every existing client keeps working); a
    /// tampered ciphertext can only make the owner's own note harder to FIND, never move it.
    #[allow(clippy::too_many_arguments)]
    ///
    /// UNCHECKED: no opening, so nothing here proves `cm` is worth `amount`. In-process
    /// callers and tests only — every public door (HTTP, relay, sigil-top's embedded API)
    /// must use [`Self::submit_shield_opened`].
    pub fn submit_shield_with_delivery(
        &self,
        from: &str,
        amount: u128,
        cm: &str,
        fee: u128,
        sig: &str,
        req_nonce: u64,
        note_ciphertext: Option<&str>,
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        self.submit_shield_core(from, amount, cm, fee, sig, req_nonce, note_ciphertext, None, None)
    }

    /// The door path for a deposit: `submit_shield_with_delivery` plus the OPENING check
    /// (`check_deposit_opening`) and the operator brake. The opening rides the relay so the
    /// next hop re-checks it rather than trusting this node's word.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_shield_opened(
        &self,
        from: &str,
        amount: u128,
        cm: &str,
        fee: u128,
        sig: &str,
        req_nonce: u64,
        note_ciphertext: Option<&str>,
        blinding: Option<&str>,
        pk_shield: Option<&str>,
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        if shield_deposits_paused() {
            return Err(ShieldedSubmitError::DepositsPaused);
        }
        check_deposit_opening(amount, &hex32(cm, "cm")?, blinding, pk_shield)?;
        self.submit_shield_core(from, amount, cm, fee, sig, req_nonce, note_ciphertext, blinding, pk_shield)
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_shield_core(
        &self,
        from: &str,
        amount: u128,
        cm: &str,
        fee: u128,
        sig: &str,
        req_nonce: u64,
        note_ciphertext: Option<&str>,
        blinding: Option<&str>,
        pk_shield: Option<&str>,
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        // v7 (2026-10-12), before any signature or nonce work: once the pool's 256-bit
        // generation is open, an old-format deposit can never land (consensus refuses it),
        // so every entry — the HTTP door, the relay, the opened variant — says so at once.
        if self.v7_active() && sigil_state::shielded::is_v1_shaped(&hex32(cm, "cm")?) {
            return Err(ShieldedSubmitError::V1DepositsClosed);
        }
        if let Some(ct) = note_ciphertext {
            let limit = sigil_shield::note_cipher::MAX_NOTE_CIPHERTEXT_LEN;
            if ct.len() > limit {
                return Err(ShieldedSubmitError::CiphertextTooLong { got: ct.len(), limit });
            }
        }
        if amount == 0 {
            return Err(ShieldedSubmitError::ZeroAmount);
        }
        check_denomination(amount)?;
        let from_b = hex32(from, "from")?;
        let cm_b = hex32(cm, "cm")?;
        // sigil-g3 chain rule, said at the door: a deposit must carry a view-tagged envelope.
        // After the cheap shape checks (the most specific error wins), before the signature.
        if sigil_header::IS_G3 && !note_ciphertext.is_some_and(sigil_shield::note_cipher::is_tagged_envelope) {
            return Err(ShieldedSubmitError::UntaggedEnvelope);
        }
        // Cheap shape validation above, signature/replay checks last — same door-guard
        // ordering the rest of this module already uses (see module docs).
        let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={req_nonce}");
        verify_wallet_sig(&from_b, &msg, sig)?;
        self.check_nonce(&from_b, req_nonce)?;
        let tx = SigilTx::Shield { from: from_b, amount, cm: cm_b, fee, note_ciphertext: note_ciphertext.map(|s| s.to_string()) };
        let id = self.enqueue(tx, None);
        self.fire_relay_hook(id, ShieldedOp::Shield(ShieldRequest {
            from: from.to_string(), amount, cm: cm.to_string(), fee, sig: sig.to_string(), req_nonce,
            note_ciphertext: note_ciphertext.map(|s| s.to_string()),
            blinding: blinding.map(|s| s.to_string()),
            pk_shield: pk_shield.map(|s| s.to_string()),
        }));
        Ok(id)
    }

    /// Shield an ARBITRARY amount by splitting it into legal denominations.
    ///
    /// Denominations are a consensus rule, but making the user perform the split is a
    /// usability failure dressed up as a security feature: a wallet holding
    /// 19,930,436,350,512 raw should not have to issue twenty-one requests to move its own
    /// balance. The split happens here, the caller makes one call, and each part is a
    /// separate note — which is what the privacy rule actually wanted anyway.
    ///
    /// `notes` supplies one commitment per part, in the order [`plan_shield`] returns. The
    /// caller derives them (only it knows the blindings), so this cannot be done for it.
    ///
    /// Wallet-authenticated as of 2026-08-23, same fix as [`Self::submit_shield`]: `sig`
    /// must verify against `from`'s own key over
    /// `sigil-rpc/v1|shield-split|{from}|{amount0}|{cm0}|{amount1}|{cm1}|...|{fee}|nonce={req_nonce}`.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_shield_split(
        &self,
        _from: &str,
        _parts: &[(u128, String)],
        _fee: u128,
        _sig: &str,
        _req_nonce: u64,
    ) -> Result<Vec<[u8; 32]>, ShieldedSubmitError> {
        // Refused since 2026-09-28: this request shape carries no per-part opening, so no
        // part's note can be checked against its amount (see `DepositOpeningMissing`). No
        // route calls it; wallets split client-side and send one opened `/v1/shield` each.
        Err(ShieldedSubmitError::DepositOpeningMissing)
    }

    /// Queue a shielded → shielded transfer. No signature: the proof authorizes it.
    ///
    /// `note_ciphertexts` (added 2026-08-24) carries a `sigil_shield::note_cipher`
    /// delivery ciphertext per output, same order as `cm_outs` — pass an empty slice
    /// for "no delivery attached to any output" or one entry per output (any of which
    /// may itself be absent, e.g. for a self-change output the sender already knows).
    /// Not part of the proof's public inputs: it rides on the transaction purely so the
    /// recipient's wallet can discover the payment without an out-of-band channel.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_shielded_send(
        &self,
        anchor: &str,
        nullifier: &str,
        extra_nullifiers: &[String],
        cm_outs: &[String],
        fee: u128,
        proof: Vec<u8>,
        note_ciphertexts: &[Option<String>],
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        if fee != sigil_state::shielded::SHIELDED_FEE {
            return Err(ShieldedSubmitError::WrongFee {
                expected: sigil_state::shielded::SHIELDED_FEE,
                got: fee,
            });
        }
        let anchor_b = hex32(anchor, "anchor")?;
        let nf = hex32(nullifier, "nullifier")?;
        let mut extra: Vec<[u8; 32]> = Vec::with_capacity(extra_nullifiers.len());
        for e in extra_nullifiers {
            let tag = hex32(e, "extra_nullifier")?;
            // DISTINCTNESS AT THE DOOR. The wallet refuses this and the chain refuses it
            // twice over, but the API is where a hand-built request arrives — and this is
            // the one check the CIRCUIT cannot make. One note offered as both inputs gives
            // two independently-valid input blocks and a conservation lane summing twice
            // the value, so the proof verifies. Only the repeated tag gives it away.
            if tag == nf || extra.contains(&tag) {
                return Err(ShieldedSubmitError::DuplicateNullifier);
            }
            extra.push(tag);
        }
        let outs = self.decode_outs(cm_outs)?;
        check_output_envelopes(outs.len(), note_ciphertexts)?;
        self.reject_if_queued(&nf)?;
        for e in &extra {
            self.reject_if_queued(e)?;
        }
        // Precheck against EVERY tag: the COUNT selects the circuit, so prechecking only
        // the first would verify a two-input proof as a one-input one — failing for the
        // wrong reason and reporting a misleading error to the caller.
        let all_nfs: Vec<[u8; 32]> = std::iter::once(nf).chain(extra.iter().copied()).collect();
        self.precheck_proof_multi(&anchor_b, &all_nfs, fee, &outs, &proof)?;

        let proof_hex = hex::encode(&proof);
        let tx = SigilTx::ShieldedSend {
            anchor: anchor_b,
            nullifier: nf,
            // The RPC surface is still 1-input. A 2-input spend needs the caller to send
            // both tags, and that request shape has not been added yet — so this stays
            // empty rather than silently dropping a second input the caller meant to spend.
            extra_nullifiers: extra,
            cm_outs: outs,
            fee,
            proof,
            note_ciphertexts: note_ciphertexts.to_vec(),
        };
        let id = self.enqueue(tx, Some(nf));
        self.fire_relay_hook(id, ShieldedOp::ShieldedSend(ShieldedSendRequest {
            anchor: anchor.to_string(),
            nullifier: nullifier.to_string(),
            // The relay must see EVERY tag. Forwarding a two-input spend as if it were
            // one-input would hand the next hop a request whose proof cannot verify — and
            // the relay is exactly where such a mistake looks like a proof bug.
            extra_nullifiers: extra_nullifiers.to_vec(),
            cm_outs: cm_outs.to_vec(),
            fee,
            proof: proof_hex,
            note_ciphertexts: note_ciphertexts.to_vec(),
        }));
        Ok(id)
    }

    /// Queue a shielded → transparent withdrawal. Proof-carrying for the same reason a
    /// shielded send is: without it, naming a nullifier would be enough to drain the pool.
    pub fn submit_unshield(
        &self,
        to: &str,
        amount: u128,
        anchor: &str,
        nullifier: &str,
        cm_outs: &[String],
        proof: Vec<u8>,
        fee: u128,
        note_ciphertexts: &[Option<String>],
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        if amount == 0 {
            return Err(ShieldedSubmitError::ZeroAmount);
        }
        check_denomination(amount)?;
        let to_b = hex32(to, "to")?;
        let anchor_b = hex32(anchor, "anchor")?;
        let nf = hex32(nullifier, "nullifier")?;
        let outs = self.decode_outs(cm_outs)?;
        // 2026-10-19: the change outputs carry delivery envelopes too — same rules as a send.
        check_output_envelopes(outs.len(), note_ciphertexts)?;
        self.reject_if_queued(&nf)?;
        // The withdrawn amount sits in the circuit's public-value slot.
        self.precheck_proof(&anchor_b, &nf, amount, &outs, &proof)?;

        let proof_hex = hex::encode(&proof);
        let tx = SigilTx::Unshield {
            to: to_b,
            amount,
            anchor: anchor_b,
            nullifier: nf,
            cm_outs: outs,
            proof,
            fee,
            note_ciphertexts: note_ciphertexts.to_vec(),
        };
        let id = self.enqueue(tx, Some(nf));
        self.fire_relay_hook(id, ShieldedOp::Unshield(UnshieldRequest {
            to: to.to_string(), amount, anchor: anchor.to_string(), nullifier: nullifier.to_string(),
            cm_outs: cm_outs.to_vec(), proof: proof_hex, fee,
            note_ciphertexts: note_ciphertexts.to_vec(),
        }));
        Ok(id)
    }

    fn decode_outs(&self, cm_outs: &[String]) -> Result<Vec<[u8; 32]>, ShieldedSubmitError> {
        let expected = sigil_shield::spend_full_v4::N_OUTS;
        if cm_outs.len() != expected {
            return Err(ShieldedSubmitError::WrongOutputCount { expected, got: cm_outs.len() });
        }
        cm_outs.iter().map(|s| hex32(s, "cm_out")).collect()
    }

    fn reject_if_queued(&self, nf: &[u8; 32]) -> Result<(), ShieldedSubmitError> {
        if self.queued_nullifiers.lock().unwrap().contains_key(nf) {
            return Err(ShieldedSubmitError::AlreadyQueued);
        }
        Ok(())
    }

    /// Door-level proof check. A DoS guard, not the security boundary — see module docs.
    /// One-input precheck. Kept as its own entry point because `submit_unshield` is
    /// always one input by construction.
    fn precheck_proof(
        &self,
        anchor: &[u8; 32],
        nf: &[u8; 32],
        public_value: u128,
        cm_outs: &[[u8; 32]],
        proof: &[u8],
    ) -> Result<(), ShieldedSubmitError> {
        self.precheck_proof_multi(anchor, std::slice::from_ref(nf), public_value, cm_outs, proof)
    }

    /// Precheck with any supported input count. The nullifier COUNT selects the circuit.
    fn precheck_proof_multi(
        &self,
        anchor: &[u8; 32],
        nfs: &[[u8; 32]],
        public_value: u128,
        cm_outs: &[[u8; 32]],
        proof: &[u8],
    ) -> Result<(), ShieldedSubmitError> {
        // (g3 port of 4e3ecd74, 2026-10-19: the g2 door refused v4 proofs inside the v1 family; on g3 the
        // v1 pool is closed and empty from genesis, so the generation dispatch below is the whole rule.)
        // v7 (2026-10-12): the ANCHOR's generation, read from the settled pool, selects the
        // verifier family — never the proof's shape. Unknown anchor ⇒ v1, as before.
        let generation = self.generation_for(anchor);
        sigil_shield::note_v7::verify_spend_wire_dual(generation, anchor, nfs, public_value, cm_outs, proof)
            .map_err(|e| ShieldedSubmitError::ProofRejected(e.to_string()))
    }

    /// Test seam: queue a tx past the door checks, exactly as `submit_*` would after
    /// verifying it. Compiled only for tests so the door stays the only production entry.
    #[cfg(test)]
    pub fn test_enqueue(&self, tx: SigilTx) -> [u8; 32] {
        self.enqueue(tx, None)
    }

    fn enqueue(&self, tx: SigilTx, nf: Option<[u8; 32]>) -> [u8; 32] {
        let hash = tx.hash();
        if let Some(nf) = nf {
            self.queued_nullifiers.lock().unwrap().insert(nf, hash);
        }
        self.pending.lock().unwrap().insert(
            hash,
            Pending { tx, attempts: 0, first_seen: Instant::now(), permanent_fails: 0, in_flight: false },
        );
        self.emit(&hash, "accepted", None);
        hash
    }

    /// Re-embed every still-pending shielded tx into the next candidate block. Called once
    /// per candidate, NOT once per settled height — same non-destructive contract as
    /// `SendBridge::snapshot_for_mint`.
    ///
    /// Offers EVERYTHING, unconditionally. Production loops must use
    /// [`Self::snapshot_for_mint_excluding`] with the frontier state instead — see its
    /// doc for the money bug the unconditional form causes on a braid.
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        self.snapshot_for_mint_excluding(|_| false)
    }

    /// Re-embed every still-pending shielded tx EXCEPT those the candidate spine already
    /// carries. `carried(tx)` answers "does the state this candidate is built on already
    /// contain this tx's effects?" — pass [`already_in_pool`] over the FRONTIER state.
    ///
    /// ## The bug this exists for (2026-09-05 live, root-caused 2026-09-08)
    ///
    /// On the braid a candidate is built on the FRONTIER: the settled state plus every
    /// unsettled block on the selected spine. Candidate N includes a `ShieldedSend`; its
    /// nullifier is now in the frontier. Candidate N+1 is built one tick later on that
    /// frontier, and the unconditional `snapshot_for_mint` re-offered the still-pending
    /// tx (nothing retires it until N crosses finality, ~512 blocks ≈ 80 s later). The
    /// builder applied it against a state that already held its nullifier, got
    /// `ShieldedRejected("nullifier already spent")`, classified that as PERMANENT, and
    /// after `REJECT_AFTER` = 3 such candidates — under a second — `note_rejection`
    /// evicted the tx and `/v1/transactions/:hash` answered **rejected** for a payment
    /// that had landed. When N finally settled, `confirm_applied` found nothing to
    /// retire, so the lie was permanent. Every client that trusted the status (Android,
    /// the MCP, sigil-top before `01eaf403`) reported failure, users retried, and the
    /// same payee was paid twice. Measured: nullifiers 7→9 for "two rejected sends".
    /// Side cost: each re-offer re-verified the STARK, hundreds of ms × every candidate
    /// until finality.
    ///
    /// The `Shield` variant had already been given exactly this treatment inside the
    /// builder (`has_ever_held(cm)` → skip quietly, 2026-08-29). This generalises it to
    /// the whole shielded family and moves it to the ONE place that knows both the tx
    /// and the frontier: a tx already on the spine is not re-offered (it would only be
    /// refused), is not counted as an attempt, and is marked `in_flight` so the status
    /// endpoint can say "in a block, awaiting finality". If the carrying block is later
    /// orphaned the next tick's frontier no longer contains it, `carried` turns false,
    /// and the tx is offered again — the orphan-safety `snapshot_for_mint` was written
    /// for is preserved exactly.
    pub fn snapshot_for_mint_excluding(&self, carried: impl Fn(&SigilTx) -> bool) -> Vec<SignedTx> {
        let mut guard = self.pending.lock().unwrap();
        let mut expired: Vec<[u8; 32]> = Vec::new();
        let mut entered_block: Vec<[u8; 32]> = Vec::new();
        let mut out = Vec::with_capacity(guard.len());
        guard.retain(|hash, p| {
            if p.attempts >= MAX_ATTEMPTS || p.first_seen.elapsed() >= MAX_AGE {
                eprintln!(
                    "✗ shielded tx gave up after {} attempts / {:.1}s hash={}",
                    p.attempts,
                    p.first_seen.elapsed().as_secs_f64(),
                    hex::encode(hash)
                );
                expired.push(*hash);
                return false;
            }
            if carried(&p.tx) {
                // Already in a candidate on the spine we are extending. Offering it
                // again can only produce a refusal against its own effects.
                if !p.in_flight {
                    entered_block.push(*hash);
                }
                p.in_flight = true;
                return true;
            }
            p.in_flight = false;
            p.attempts += 1;
            out.push(to_signed(p.tx.clone()));
            true
        });
        drop(guard);
        if !expired.is_empty() {
            self.forget_nullifiers(&expired);
            for h in &expired {
                self.emit(h, "rejected", Some("gave up: not included before the pending limit".into()));
            }
        }
        for h in &entered_block {
            self.emit(h, "in_block", None);
        }
        out
    }

    /// Retire every pending tx whose effects are in the SETTLED state — pass
    /// [`already_in_pool`] over the settled chain's pool. This is the finality confirm
    /// for the case `confirm_applied` cannot see: a tx that landed inside a block THIS
    /// node did not mint (a peer's candidate carried the relayed copy), which never gets
    /// an entry in the producer's mint-hash → tx-hash map and so used to sit pending for
    /// the full `MAX_AGE` (40 min) before being reported as "gave up" — for money that
    /// had settled. Returns how many were retired.
    pub fn retire_settled(&self, settled: impl Fn(&SigilTx) -> bool) -> usize {
        let landed: Vec<[u8; 32]> = {
            let mut guard = self.pending.lock().unwrap();
            let landed: Vec<[u8; 32]> =
                guard.iter().filter(|(_, p)| settled(&p.tx)).map(|(h, _)| *h).collect();
            for h in &landed {
                guard.remove(h);
            }
            landed
        };
        if !landed.is_empty() {
            self.forget_nullifiers(&landed);
            self.remember(landed.iter().map(|h| (*h, TxOutcome::Applied)));
        }
        landed.len()
    }

    /// Retire landed shielded txs.
    pub fn confirm_applied(&self, hashes: &[[u8; 32]]) {
        if hashes.is_empty() {
            return;
        }
        let landed: Vec<[u8; 32]> = {
            let mut guard = self.pending.lock().unwrap();
            hashes.iter().filter(|h| guard.remove(*h).is_some()).copied().collect()
        };
        self.forget_nullifiers(hashes);
        if !landed.is_empty() {
            self.remember(landed.iter().map(|h| (*h, TxOutcome::Applied)));
        }
    }

    /// The candidate builder could not apply `hash` for a reason no later candidate can
    /// cure. After `REJECT_AFTER` such verdicts the tx is evicted, its nullifier freed, and
    /// the reason kept for `/v1/transactions/:hash`. Non-permanent failures never reach here.
    pub fn note_rejection(&self, hash: [u8; 32], reason: &str) {
        let evicted = {
            let mut guard = self.pending.lock().unwrap();
            match guard.get_mut(&hash) {
                None => false,
                Some(p) => {
                    p.permanent_fails += 1;
                    if p.permanent_fails >= REJECT_AFTER {
                        eprintln!(
                            "✗ shielded tx evicted after {} permanent refusals ({reason}) hash={}",
                            p.permanent_fails, hex::encode(hash)
                        );
                        guard.remove(&hash);
                        true
                    } else {
                        false
                    }
                }
            }
        };
        if evicted {
            self.forget_nullifiers(&[hash]);
            self.remember(std::iter::once((hash, TxOutcome::Rejected(reason.to_string()))));
        }
    }

    /// What is known about `hash`: pending, applied, rejected — or nothing.
    pub fn status(&self, hash: &[u8; 32]) -> Option<TxOutcome> {
        if let Some(p) = self.pending.lock().unwrap().get(hash) {
            return Some(TxOutcome::Pending {
                attempts: p.attempts,
                permanent_fails: p.permanent_fails,
                in_flight: p.in_flight,
            });
        }
        self.outcomes.lock().unwrap().get(hash).map(|(o, _)| o.clone())
    }

    fn remember(&self, items: impl Iterator<Item = ([u8; 32], TxOutcome)>) {
        let items: Vec<([u8; 32], TxOutcome)> = items.collect();
        let mut g = self.outcomes.lock().unwrap();
        let now = Instant::now();
        for (h, o) in &items { g.insert(*h, (o.clone(), now)); }
        if g.len() > OUTCOME_CAP || g.len() % 256 == 0 {
            g.retain(|_, (_, t)| t.elapsed() < OUTCOME_TTL);
        }
        if g.len() > OUTCOME_CAP {
            // Oldest first, never the whole map: an answer is worth more than a byte.
            let mut v: Vec<([u8; 32], Instant)> = g.iter().map(|(h, (_, t))| (*h, *t)).collect();
            v.sort_by_key(|(_, t)| *t);
            let excess = g.len() - OUTCOME_CAP;
            for (h, _) in v.into_iter().take(excess) { g.remove(&h); }
        }
        drop(g);
        for (h, o) in &items {
            match o {
                TxOutcome::Applied => self.emit(h, "applied", None),
                TxOutcome::Rejected(r) => self.emit(h, "rejected", Some(r.clone())),
                TxOutcome::Pending { .. } => {}
            }
        }
    }

    /// Release the queued-nullifier reservations held by these tx hashes, so a note whose
    /// transaction expired can be respent rather than being locked out of the queue
    /// forever.
    fn forget_nullifiers(&self, hashes: &[[u8; 32]]) {
        let mut q = self.queued_nullifiers.lock().unwrap();
        q.retain(|_, h| !hashes.contains(h));
    }

    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Is a `RegisterShieldedAddress` for `wallet` still in this bridge? Lets the address
    /// lookup say "publishing right now, wait" instead of "unregistered" during the
    /// ~90 s between a wallet's first open and its registration crossing finality — the
    /// exact window in which a phone tried to pay a freshly opened browser wallet
    /// (2026-09-08 13:47) and was told, truthfully but uselessly, that it had no key.
    pub fn pending_registration(&self, wallet: &WalletId) -> Option<TxOutcome> {
        let guard = self.pending.lock().unwrap();
        guard.values().find_map(|p| match &p.tx {
            SigilTx::RegisterShieldedAddress { wallet: w, .. } if w == wallet => Some(TxOutcome::Pending {
                attempts: p.attempts,
                permanent_fails: p.permanent_fails,
                in_flight: p.in_flight,
            }),
            _ => None,
        })
    }

    /// 2026-09-09: the keys a still-pending registration carries, so `/v1/shielded/address`
    /// can answer a payer BEFORE the registration settles. The registration was signed by
    /// the wallet itself when this pool accepted it, so paying these keys pays that wallet
    /// whether or not the registration has landed yet — a shielded payment needs only the
    /// keys, never the settled registration. Returns (pk_shield, pk_encrypt, in_flight).
    pub fn pending_registration_keys(&self, wallet: &WalletId) -> Option<([u8; 32], Option<[u8; 32]>, bool)> {
        let guard = self.pending.lock().unwrap();
        guard.values().find_map(|p| match &p.tx {
            SigilTx::RegisterShieldedAddress { wallet: w, pk_shield, pk_encrypt, .. } if w == wallet => {
                Some((*pk_shield, *pk_encrypt, p.in_flight))
            }
            _ => None,
        })
    }
}

// ── request shapes ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct RegisterRequest {
    pub wallet: String,
    /// Hex of the wire-encoded shielded public key (`ShieldedAccount::public_key`).
    pub pk_shield: String,
    /// Hex X25519 note-delivery key (`ShieldedAccount::address`'s `pk_enc`). Required:
    /// a registration with no delivery key can still receive privately, but nothing
    /// could ever tell that wallet a payment landed — it would have to be told out of
    /// band, which is the exact gap this whole feature closes.
    pub pk_encrypt: String,
    #[serde(default, with = "sigil_state::u128_str")]
    pub fee: u128,
    /// 128-hex Ed25519 signature over
    /// `sigil-rpc/v1|shield-register|{wallet}|{pk_shield}|{pk_encrypt}|{fee}|nonce={req_nonce}`.
    pub sig: String,
    /// Optional SQIsign L5 public key, hex (129 bytes → 258 hex chars). Upgrades this
    /// wallet's shielded-RAMP authorization to post-quantum. Omit it and every existing
    /// client behaves exactly as before.
    #[serde(default)]
    pub pk_sqi: Option<String>,
    /// Proof of possession: a SQIsign signature, BY `pk_sqi` itself, over
    /// `sigil-rpc/v1|shield-sqi-pop|{wallet}|{pk_sqi}`.
    ///
    /// Required whenever `pk_sqi` is present; a key without it is refused rather than
    /// stored. Without this check, an adversary who can forge the Ed25519 half — exactly
    /// the adversary this feature exists to stop — could bind THEIR key to YOUR wallet and
    /// take sole control of its ramps.
    #[serde(default)]
    pub sqi_pop: Option<String>,
    /// Client-chosen strictly-increasing nonce, same convention as `/v1/send`.
    pub req_nonce: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ShieldRequest {
    pub from: String,
    #[serde(with = "sigil_state::u128_str")]
    pub amount: u128,
    /// `compress2(amount, blinding)` hex — the depositor computes this locally and keeps
    /// the blinding. The server never learns it, which is what makes the note private.
    pub cm: String,
    #[serde(default, with = "sigil_state::u128_str")]
    pub fee: u128,
    /// 128-hex Ed25519 signature over
    /// `sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={req_nonce}`.
    pub sig: String,
    /// Client-chosen strictly-increasing nonce, same convention as `/v1/send`.
    pub req_nonce: u64,
    /// Optional: the note sealed to the depositor's own delivery key (see `submit_shield_with_delivery`).
    ///
    /// `default` only — NEVER `skip_serializing_if`. This struct is the JSON body of
    /// `/v1/shield`, but it ALSO rides TOPIC_TXS and the Dandelion stem hops as bincode
    /// inside `RelayedTx::Shielded(ShieldedOp::Shield(..))`, and bincode cannot decode a
    /// skipped field: with `None` the encoder wrote zero bytes, the peer's decoder read an
    /// Option tag past the end (`unexpected end of file`) and dropped the deposit at
    /// `apply_locally`'s `else { return }` — a deposit submitted at a follower never reached
    /// the producer. Added 6dde106c (2026-09-08), found by `fluxc wire-audit` 2026-09-13,
    /// pinned by `dandelion_relay::wire_tests`. A JSON `null` here costs 22 bytes.
    #[serde(default)]
    pub note_ciphertext: Option<String>,
    /// The deposit's OPENING (2026-09-28): the note's blinding, wire hex. With `pk_shield`
    /// it lets the door recompute `cm = compress2(compress2(amount, blinding), pk_shield)`
    /// and refuse a deposit whose note is worth more than the amount it debits. Same
    /// bincode rule as `note_ciphertext`: `default` only, never `skip_serializing_if`.
    #[serde(default)]
    pub blinding: Option<String>,
    /// The note owner's published circuit key (`GET /v1/shielded/address`), wire hex.
    #[serde(default)]
    pub pk_shield: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ShieldedSendRequest {
    pub anchor: String,
    pub nullifier: String,
    /// Hex tags for inputs BEYOND the first. Omit or pass `[]` for a one-note spend; one
    /// entry for a two-note merge — the only way to spend more than a wallet's largest
    /// single note.
    ///
    /// Refused before `sigil_tx::SHIELDED_MULTI_INPUT_HEIGHT`, and refused outright if it
    /// repeats `nullifier`.
    #[serde(default)]
    pub extra_nullifiers: Vec<String>,
    pub cm_outs: Vec<String>,
    #[serde(with = "sigil_state::u128_str")]
    pub fee: u128,
    /// Hex-encoded winterfell proof.
    pub proof: String,
    /// Per-output `sigil_shield::note_cipher::NoteCiphertext` JSON, same order as
    /// `cm_outs`. Pass `[]` for no delivery attached to any output, or one entry per
    /// output (`null` for an output the recipient will discover another way, e.g. the
    /// sender's own change). This is what lets a recipient who was never told anything
    /// out of band still discover a payment by trial-decryption.
    #[serde(default)]
    pub note_ciphertexts: Vec<Option<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UnshieldRequest {
    pub to: String,
    #[serde(with = "sigil_state::u128_str")]
    pub amount: u128,
    pub anchor: String,
    pub nullifier: String,
    pub cm_outs: Vec<String>,
    pub proof: String,
    #[serde(default, with = "sigil_state::u128_str")]
    pub fee: u128,
    /// Per-change-output envelope, same rules as [`ShieldedSendRequest::note_ciphertexts`]
    /// (2026-10-19; required on sigil-g3). `default` only — the bincode relay rule.
    #[serde(default)]
    pub note_ciphertexts: Vec<Option<String>>,
}

/// The delivery-envelope rules every proof-carrying spend shares (send AND unshield):
/// empty or one entry per output, none larger than an honest sealed note, and — on the
/// sigil-g3 family (chain rule, 2026-10-19) — exactly one view-tagged envelope per output.
fn check_output_envelopes(outs: usize, note_ciphertexts: &[Option<String>]) -> Result<(), ShieldedSubmitError> {
    if !note_ciphertexts.is_empty() && note_ciphertexts.len() != outs {
        return Err(ShieldedSubmitError::WrongCiphertextCount { expected: outs, got: note_ciphertexts.len() });
    }
    let limit = sigil_shield::note_cipher::MAX_NOTE_CIPHERTEXT_LEN;
    for ct in note_ciphertexts.iter().flatten() {
        if ct.len() > limit {
            return Err(ShieldedSubmitError::CiphertextTooLong { got: ct.len(), limit });
        }
    }
    if sigil_header::IS_G3
        && (note_ciphertexts.len() != outs
            || !note_ciphertexts.iter().all(|c| c.as_deref().is_some_and(sigil_shield::note_cipher::is_tagged_envelope)))
    {
        return Err(ShieldedSubmitError::UntaggedEnvelope);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// sigil-g3 chain rule: every deposit and spend output carries a view-tagged envelope. Tests
    /// that exercise the door's OTHER rules attach a well-formed one on the g3 family (and none
    /// on g2, exactly as before) — 2026-10-19.
    fn g3_envelope() -> Option<String> {
        sigil_header::IS_G3.then(|| {
            let a = sigil_shield::wallet_v7::AccountV7::from_seed([7u8; 32]);
            sigil_shield::wallet_v7::seal_output_v7(1, a.blinding(0), &a.pk_enc_hex(), "").unwrap().0
        })
    }
    fn g3_envelopes(n: usize) -> Vec<Option<String>> {
        if sigil_header::IS_G3 { vec![g3_envelope(); n] } else { Vec::new() }
    }
    use super::*;

    /// A fixed window onto the pool, for door tests.
    struct FixedView(Option<sigil_shield::note_v7::PoolGeneration>, bool);
    impl PoolView for FixedView {
        fn generation_of_anchor(&self, _: &[u8; 32]) -> Option<sigil_shield::note_v7::PoolGeneration> { self.0 }
        fn v7_active(&self) -> bool { self.1 }
    }

    /// v7 DEPOSIT OPENING: a 256-bit commitment opens with a 256-bit blinding and owner key;
    /// a wrong amount, blinding or owner is refused; the v1 path is untouched.
    #[test]
    fn a_v7_deposit_opening_is_checked_in_v7_form() {
        use sigil_shield::note_v7::{Digest4, NoteV7};
        use sigil_shield::spend_full_v7::OutputV7;
        let owner = NoteV7::new(0, [0; 4], [7, 7, 7, 7]).unwrap().owner_pk();
        let blinding = Digest4::from_u64s([11, 22, 33, 44]);
        let out = OutputV7::new(10_000_000_000, [11, 22, 33, 44], owner);
        let cm = out.commitment().to_wire();
        assert!(!sigil_state::shielded::is_v1_shaped(&cm));
        let (b, pk) = (hex::encode(blinding.to_wire()), hex::encode(owner.to_wire()));
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, Some(&b), Some(&pk)), Ok(()));
        assert_eq!(check_deposit_opening(20_000_000_000, &cm, Some(&b), Some(&pk)), Err(ShieldedSubmitError::DepositOpeningMismatch));
        let other = hex::encode(NoteV7::new(0, [0; 4], [8, 8, 8, 8]).unwrap().owner_pk().to_wire());
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, Some(&b), Some(&other)), Err(ShieldedSubmitError::DepositOpeningMismatch));
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, None, Some(&pk)), Err(ShieldedSubmitError::DepositOpeningMissing));
        // a non-canonical limb in the blinding is a mismatch, never a panic
        let mut bad = blinding.to_wire();
        bad[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, Some(&hex::encode(bad)), Some(&pk)), Err(ShieldedSubmitError::DepositOpeningMismatch));
    }

    /// Once the pool's v7 generation is open, an old-format deposit is refused at the door
    /// before any signature or opening work; without a pool view (or before activation) the
    /// same request takes the old path and fails for the old reasons.
    #[test]
    fn old_format_deposits_are_refused_at_the_door_once_v7_is_open() {
        let b = ShieldedBridge::new();
        let mut v1 = [0u8; 32];
        v1[..8].copy_from_slice(&42u64.to_le_bytes());
        let cm = hex::encode(v1);
        let from = hex::encode([1u8; 32]);
        let before = b.submit_shield_with_delivery(&from, 1_000, &cm, 0, "00", 1, None);
        assert!(!matches!(before, Err(ShieldedSubmitError::V1DepositsClosed)), "no view ⇒ v1 behaviour: {before:?}");
        b.set_pool_view(FixedView(None, true));
        assert_eq!(
            b.submit_shield_with_delivery(&from, 1_000, &cm, 0, "00", 2, None),
            Err(ShieldedSubmitError::V1DepositsClosed)
        );
        assert!(ShieldedSubmitError::V1DepositsClosed.message().contains("Update the wallet"));
    }

    /// THE DOOR ROUTES BY GENERATION: a real v7 spend passes the precheck when the pool says
    /// its anchor is v7, and is refused when the pool says v1 (or knows nothing) — the proof's
    /// own shape never decides.
    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn door_precheck_routes_the_proof_by_the_anchors_generation() {
        use sigil_shield::note_v7::{padding_leaf, NoteV7, PoolGeneration, PoolTreeV7};
        use sigil_shield::spend_full_v7::{build_spend_full_v7_trace_seeded_u64, v7_options, OutputV7, SpendFullV7Prover, DEPTH};
        use sigil_shield::zk_mask::HidingProver;
        let fee = sigil_state::shielded::SHIELDED_FEE;
        let note = NoteV7::new(5_000_000, [1, 2, 3, 4], [5, 6, 7, 8]).unwrap();
        let bob = NoteV7::new(0, [0; 4], [9, 9, 9, 9]).unwrap().owner_pk();
        let outs = [OutputV7::new(1_000_000, [11, 12, 13, 14], bob), OutputV7::new(5_000_000 - 1_000_000 - fee as u64, [21, 22, 23, 24], note.owner_pk())];
        let pos = 77usize;
        let mut leaves = vec![padding_leaf(); 1 << DEPTH];
        leaves[pos] = note.commitment();
        let tree = PoolTreeV7::new(leaves).unwrap();
        let path = tree.path(pos).unwrap();
        let t = build_spend_full_v7_trace_seeded_u64(&note, &path, fee as u64, &outs, &v7_options(), [3u8; 32]);
        let proof = SpendFullV7Prover::new(v7_options()).prove_hiding(t).unwrap().to_bytes();
        let anchor = hex::encode(tree.root().to_wire());
        let nf = hex::encode(note.nullifier(pos as u64).to_wire());
        let cms: Vec<String> = outs.iter().map(|o| hex::encode(o.commitment().to_wire())).collect();

        let b = ShieldedBridge::new();
        let refused = b.submit_shielded_send(&anchor, &nf, &[], &cms, fee, proof.clone(), &g3_envelopes(2));
        assert!(matches!(refused, Err(ShieldedSubmitError::ProofRejected(_))), "no view ⇒ v1 verifier ⇒ refused: {refused:?}");
        b.set_pool_view(FixedView(Some(PoolGeneration::V1), true));
        assert!(matches!(b.submit_shielded_send(&anchor, &nf, &[], &cms, fee, proof.clone(), &g3_envelopes(2)), Err(ShieldedSubmitError::ProofRejected(_))));
        b.set_pool_view(FixedView(Some(PoolGeneration::V7), true));
        b.submit_shielded_send(&anchor, &nf, &[], &cms, fee, proof, &g3_envelopes(2)).expect("a v7 spend passes the door when its anchor is v7");
    }


    /// The brake is opt-in: only `SIGIL_SHIELD_DEPOSITS=pause` pauses deposits. Reading
    /// only (never `set_var`), so it cannot race the rest of the suite.
    #[test]
    fn the_deposit_brake_is_opt_in() {
        if std::env::var("SIGIL_SHIELD_DEPOSITS").as_deref() != Ok("pause") {
            assert!(!shield_deposits_paused());
        }
        assert!(ShieldedSubmitError::DepositsPaused.message().contains("paused"));
    }

    fn opening(value: u64, index: u64) -> ([u8; 32], String, String) {
        use sigil_shield::note_v1::to_wire;
        let acct = sigil_shield::wallet::ShieldedAccount::from_seed([7u8; 32]);
        let note = acct.note(index, value).expect("value in range");
        (
            to_wire(note.commitment()),
            hex::encode(to_wire(acct.blinding(index))),
            hex::encode(to_wire(acct.public_key())),
        )
    }

    /// The wallet's own derivation (`ShieldedAccount::note`, what `shieldNoteCommitment`
    /// exports to the browser) must satisfy the door, byte for byte.
    #[test]
    fn an_honest_deposit_opens_to_its_own_commitment() {
        let (cm, b, pk) = opening(10_000_000_000, 3);
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, Some(&b), Some(&pk)), Ok(()));
    }

    /// THE hole: debit the smallest amount while the note says far more. The spend proof
    /// would later be honest about the note and pay the difference out of the pool.
    #[test]
    fn a_small_deposit_carrying_a_big_note_is_refused() {
        let (cm_big, b, pk) = opening(1u64 << 57, 0);
        assert_eq!(
            check_deposit_opening(100_000, &cm_big, Some(&b), Some(&pk)),
            Err(ShieldedSubmitError::DepositOpeningMismatch)
        );
    }

    #[test]
    fn a_deposit_owned_by_another_key_or_blinding_is_refused() {
        let (cm, b, _pk) = opening(10_000_000_000, 1);
        let other_b = opening(10_000_000_000, 2).1;
        let other = hex::encode(sigil_shield::note_v1::to_wire(
            sigil_shield::wallet::ShieldedAccount::from_seed([9u8; 32]).public_key(),
        ));
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, Some(&b), Some(&other)), Err(ShieldedSubmitError::DepositOpeningMismatch));
        let pk = opening(10_000_000_000, 1).2;
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, Some(&other_b), Some(&pk)), Err(ShieldedSubmitError::DepositOpeningMismatch));
    }

    /// Old wallets send no opening: refused with an "update your wallet" message, never
    /// silently accepted. Non-canonical spellings are refused, not normalised.
    #[test]
    fn a_deposit_without_a_canonical_opening_is_refused() {
        let (cm, b, pk) = opening(10_000_000_000, 4);
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, None, Some(&pk)), Err(ShieldedSubmitError::DepositOpeningMissing));
        assert_eq!(check_deposit_opening(10_000_000_000, &cm, Some(&b), None), Err(ShieldedSubmitError::DepositOpeningMissing));
        let dirty = format!("{}01", &b[..62]); // non-zero padding byte
        assert!(check_deposit_opening(10_000_000_000, &cm, Some(&dirty), Some(&pk)).is_err());
        assert!(ShieldedSubmitError::DepositOpeningMissing.message().contains("Update"));
    }

    /// The split shape has no per-part opening, so it is refused outright.
    #[test]
    fn the_unopened_split_path_is_closed() {
        let b = ShieldedBridge::new();
        assert_eq!(
            b.submit_shield_split(&"aa".repeat(32), &[(100_000, "bb".repeat(32))], 0, "", 1),
            Err(ShieldedSubmitError::DepositOpeningMissing)
        );
    }

    /// A tx the builder refuses for a permanent reason is evicted after `REJECT_AFTER`
    /// verdicts, its nullifier is released, and the reason is answerable afterwards.
    #[test]
    fn permanent_rejections_evict_and_are_remembered() {
        let b = ShieldedBridge::new();
        let tx = SigilTx::Shield { from: [7u8; 32], amount: 200_000, cm: [9u8; 32], fee: 0, note_ciphertext: None };
        let h = b.enqueue(tx, Some([5u8; 32]));
        assert!(matches!(b.status(&h), Some(TxOutcome::Pending { .. })));
        for _ in 0..(REJECT_AFTER - 1) { b.note_rejection(h, "apply_tx: insufficient balance"); }
        assert_eq!(b.pending_len(), 1, "one verdict short of eviction must still be pending");
        b.note_rejection(h, "apply_tx: insufficient balance");
        assert_eq!(b.pending_len(), 0);
        assert_eq!(b.status(&h), Some(TxOutcome::Rejected("apply_tx: insufficient balance".into())));
        assert!(b.queued_nullifiers.lock().unwrap().is_empty(), "the note must be respendable");
        b.note_rejection([1u8; 32], "x");
        assert_eq!(b.status(&[1u8; 32]), None);
    }

    #[test]
    fn applied_is_remembered_after_confirm() {
        let b = ShieldedBridge::new();
        let tx = SigilTx::Shield { from: [7u8; 32], amount: 200_000, cm: [8u8; 32], fee: 0, note_ciphertext: None };
        let h = b.enqueue(tx, None);
        b.confirm_applied(&[h]);
        assert_eq!(b.status(&h), Some(TxOutcome::Applied));
        assert_eq!(b.pending_len(), 0);
    }

    /// The push side (2026-09-12): every transition `status()` can report is also
    /// published, in order, on the wired bus — accepted at the door, in_block when the
    /// frontier carries it, applied when it settles. A wallet on `/wait` or `/v1/events`
    /// therefore never learns less, or later, than one that polls.
    #[tokio::test]
    async fn every_status_transition_is_pushed_on_the_bus_in_order() {
        use crate::events::{Event, EventBus};
        let b = ShieldedBridge::new();
        let bus = Arc::new(EventBus::new());
        b.set_events(bus.clone());
        let mut rx = bus.subscribe();
        let tx = SigilTx::Shield { from: [7u8; 32], amount: 200_000, cm: [8u8; 32], fee: 0, note_ciphertext: None };
        let h = b.enqueue(tx, None);
        // first candidate: offered (not yet carried) → no in_block yet
        assert_eq!(b.snapshot_for_mint_excluding(|_| false).len(), 1);
        // second candidate: the frontier now carries it → in_block, exactly once
        assert_eq!(b.snapshot_for_mint_excluding(|_| true).len(), 0);
        assert_eq!(b.snapshot_for_mint_excluding(|_| true).len(), 0);
        b.confirm_applied(&[h]);
        let mut seen = Vec::new();
        while let Ok(item) = rx.try_recv() {
            if let Event::Tx { tx_hash, status, .. } = &item.event {
                assert_eq!(tx_hash, &hex::encode(h));
                seen.push(status.clone());
            }
        }
        assert_eq!(seen, vec!["accepted", "in_block", "applied"]);
    }

    #[tokio::test]
    async fn an_eviction_is_pushed_as_rejected_with_the_reason() {
        use crate::events::{Event, EventBus};
        let b = ShieldedBridge::new();
        let bus = Arc::new(EventBus::new());
        b.set_events(bus.clone());
        let mut rx = bus.subscribe();
        let tx = SigilTx::Shield { from: [1u8; 32], amount: 200_000, cm: [2u8; 32], fee: 0, note_ciphertext: None };
        let h = b.enqueue(tx, None);
        for _ in 0..REJECT_AFTER { b.note_rejection(h, "unknown anchor"); }
        let mut last = None;
        while let Ok(item) = rx.try_recv() {
            if let Event::Tx { status, reason, .. } = &item.event { last = Some((status.clone(), reason.clone())); }
        }
        assert_eq!(last, Some(("rejected".to_string(), Some("unknown anchor".to_string()))));
        assert!(matches!(b.status(&h), Some(TxOutcome::Rejected(_))));
    }

    /// Guard-rail for the defect that silently disabled this whole path: `MAX_AGE` must
    /// exceed the time a candidate needs to become eligible to SETTLE, or nothing
    /// submitted here can ever complete. Deliberately restates its inputs rather than
    /// importing them — `sigil-api` does not depend on `sigil-dagknight`, and the point
    /// is to fail loudly if someone re-derives these without redoing this arithmetic.
    /// `final_depth = 512` at the measured live block rate (6.28 blk/s, 2026-08-26)
    /// = ~81.5s to the EARLIEST possible settlement. The old 60s was below that floor.
    #[test]
    fn max_age_clears_the_worst_case_finality_lag() {
        const FINAL_DEPTH: u64 = 512;
        const SLOWEST_MEASURED_BLOCK_RATE_PER_SEC: u64 = 6;
        let earliest_settlement_secs = FINAL_DEPTH / SLOWEST_MEASURED_BLOCK_RATE_PER_SEC;
        assert!(
            MAX_AGE.as_secs() > earliest_settlement_secs,
            "MAX_AGE {}s is at or below the {}s floor before a candidate can settle — \
             at this value NO tx on this path can ever complete",
            MAX_AGE.as_secs(),
            earliest_settlement_secs,
        );
    }

    /// `MAX_ATTEMPTS` must not bind before `MAX_AGE`. This is not hypothetical: raising
    /// only `MAX_AGE` and leaving the attempt cap is exactly how the shielded path stayed
    /// broken after its own fix (`gave up after 600 attempts / 92.0s`). Offer cadence is
    /// one per candidate mint, ~6.7/s measured, so budget against the fastest plausible.
    #[test]
    fn max_attempts_does_not_bind_before_max_age() {
        const FASTEST_OFFER_CADENCE_PER_SEC: u64 = 9;
        assert!(
            u64::from(MAX_ATTEMPTS) >= MAX_AGE.as_secs() * FASTEST_OFFER_CADENCE_PER_SEC,
            "MAX_ATTEMPTS {} cuts this path off after ~{}s, before MAX_AGE {}s",
            MAX_ATTEMPTS,
            u64::from(MAX_ATTEMPTS) / FASTEST_OFFER_CADENCE_PER_SEC,
            MAX_AGE.as_secs(),
        );
    }

    /// THE ROOT-CAUSE GATE for the "registration never lands" bug (2026-08-24).
    ///
    /// Measured live against the real production DAGKnight braid, not estimated:
    /// `final_depth=512` at this chain's real ~3.5s block time puts finality
    /// ~500 blocks / ~29 minutes behind a freshly-proposed candidate
    /// (`dag_drain_apply` in `sigil-node/src/dag.rs` only state-applies blocks
    /// once they cross that threshold). A pending shielded tx that gives up
    /// before a candidate carrying it could ever reach that threshold can
    /// mathematically never land — reproduced live: a real registration gave up
    /// at "19 attempts / 122.3s" while `finalized_height` was still ~488 blocks
    /// behind where that tx's candidate was proposed.
    ///
    /// This pins the fix at the type level: if `MAX_AGE` or `MAX_ATTEMPTS` ever
    /// regress below the real finality floor again (e.g. someone "cleans up" the
    /// constant back toward `send`'s original 60s without re-deriving it), this
    /// test catches it before it ships, rather than silently reintroducing a bug
    /// that took a live reproduction to find the first time.
    #[test]
    fn retry_budget_exceeds_the_real_finality_floor() {
        const MEASURED_FINAL_DEPTH: u64 = 512;
        const MEASURED_BLOCK_SECS: u64 = 4; // ~3.5s measured live, rounded up for margin
        let finality_floor = Duration::from_secs(MEASURED_FINAL_DEPTH * MEASURED_BLOCK_SECS);
        assert!(
            MAX_AGE > finality_floor,
            "MAX_AGE ({MAX_AGE:?}) must exceed the real finality floor ({finality_floor:?}) \
             or NO shielded tx can ever land — see the 2026-08-24 root-cause writeup"
        );
        // Attempts accumulate roughly once per candidate mint (one per block, not
        // per drain tick) — must comfortably cover the same window MAX_AGE does.
        assert!(
            (MAX_ATTEMPTS as u64) >= MEASURED_FINAL_DEPTH,
            "MAX_ATTEMPTS ({MAX_ATTEMPTS}) must cover at least one full final_depth \
             window ({MEASURED_FINAL_DEPTH}) of candidate-mint attempts"
        );
    }

    fn signer() -> (ed25519_dalek::SigningKey, String) {
        let sk = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let addr = hex::encode(sk.verifying_key().to_bytes());
        (sk, addr)
    }

    fn sign_shield(sk: &ed25519_dalek::SigningKey, from: &str, amount: u128, cm: &str, fee: u128, nonce: u64) -> String {
        use ed25519_dalek::Signer;
        let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={nonce}");
        hex::encode(sk.sign(msg.as_bytes()).to_bytes())
    }

    fn sign_register(
        sk: &ed25519_dalek::SigningKey,
        wallet: &str,
        pk_shield: &str,
        pk_encrypt: &str,
        fee: u128,
        nonce: u64,
    ) -> String {
        use ed25519_dalek::Signer;
        let msg = format!(
            "sigil-rpc/v1|shield-register|{wallet}|{pk_shield}|{pk_encrypt}|{fee}|nonce={nonce}"
        );
        hex::encode(sk.sign(msg.as_bytes()).to_bytes())
    }

    #[test]
    fn shield_rejects_zero_and_bad_hex() {
        let b = ShieldedBridge::new();
        assert_eq!(
            b.submit_shield_with_delivery("aa", 0, "bb", 0, "", 1, g3_envelope().as_deref()).unwrap_err(),
            ShieldedSubmitError::ZeroAmount
        );
        assert!(matches!(
            b.submit_shield_with_delivery("zz", 1_000, &"11".repeat(32), 0, "", 1, g3_envelope().as_deref()).unwrap_err(),
            ShieldedSubmitError::BadHex("from")
        ));
        assert!(matches!(
            b.submit_shield_with_delivery(&"aa".repeat(32), 1_000, "beef", 0, "", 1, g3_envelope().as_deref()).unwrap_err(),
            ShieldedSubmitError::BadLength { field: "cm", .. }
        ));
    }

    /// THE FIX, pinned directly: naming someone else's wallet without their signature
    /// must be refused, not silently accepted. Before 2026-08-23 this test would have
    /// FAILED to compile as a rejection — the old `submit_shield` had no signature
    /// parameter to get wrong, which was the entire vulnerability.
    #[test]
    fn shield_refuses_an_unsigned_or_wrongly_signed_request() {
        let b = ShieldedBridge::new();
        let (_owner_sk, owner) = signer();
        let (attacker_sk, _attacker) = signer();
        // No signature at all.
        assert_eq!(
            b.submit_shield_with_delivery(&owner, 10_000, &"bb".repeat(32), 1, "", 1, g3_envelope().as_deref()).unwrap_err(),
            ShieldedSubmitError::SignatureInvalid
        );
        // Signed by someone who is NOT the named wallet — the actual attack this fix
        // closes: knowing `owner`'s address is not the same as owning it.
        let forged = sign_shield(&attacker_sk, &owner, 10_000, &"bb".repeat(32), 1, 1);
        assert_eq!(
            b.submit_shield_with_delivery(&owner, 10_000, &"bb".repeat(32), 1, &forged, 1, g3_envelope().as_deref()).unwrap_err(),
            ShieldedSubmitError::SignatureInvalid
        );
    }

    #[test]
    fn shield_enqueues_and_retires() {
        let b = ShieldedBridge::new();
        let (sk, from) = signer();
        let cm = "bb".repeat(32);
        let sig = sign_shield(&sk, &from, 10_000, &cm, 1, 1);
        let h = b.submit_shield_with_delivery(&from, 10_000, &cm, 1, &sig, 1, g3_envelope().as_deref()).expect("queued");
        assert_eq!(b.pending_len(), 1);
        assert_eq!(b.snapshot_for_mint().len(), 1, "re-embedded until confirmed");
        assert_eq!(b.pending_len(), 1, "snapshot must NOT be destructive");
        b.confirm_applied(&[h]);
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn shield_replayed_nonce_is_refused() {
        let b = ShieldedBridge::new();
        let (sk, from) = signer();
        let cm = "bb".repeat(32);
        let sig = sign_shield(&sk, &from, 10_000, &cm, 1, 5);
        b.submit_shield_with_delivery(&from, 10_000, &cm, 1, &sig, 5, g3_envelope().as_deref()).expect("first use of nonce 5");
        // Same nonce again, even with a validly re-signed message, must be refused.
        let sig2 = sign_shield(&sk, &from, 10_000, &cm, 1, 5);
        assert_eq!(
            b.submit_shield_with_delivery(&from, 10_000, &cm, 1, &sig2, 5, g3_envelope().as_deref()).unwrap_err(),
            ShieldedSubmitError::ReplayedNonce
        );
    }

    /// Same vulnerability class as `shield`, same fix: registering redirects a wallet's
    /// FUTURE income, so only that wallet's own key may do it.
    #[test]
    fn register_refuses_an_unowned_wallet() {
        let b = ShieldedBridge::new();
        let (_owner_sk, owner) = signer();
        let (attacker_sk, _attacker) = signer();
        let pk_shield = "cc".repeat(32);
        let pk_encrypt = "dd".repeat(32);
        let forged = sign_register(&attacker_sk, &owner, &pk_shield, &pk_encrypt, 0, 1);
        assert_eq!(
            b.submit_register(&owner, &pk_shield, &pk_encrypt, 0, &forged, 1, None, None).unwrap_err(),
            ShieldedSubmitError::SignatureInvalid,
            "an attacker must not be able to redirect someone else's future mining rewards"
        );
    }

    /// Swapping in a different `pk_encrypt` on an otherwise-owner-signed request must be
    /// refused too — this is the delivery-key hijack the message-binding fix closes.
    #[test]
    fn register_refuses_a_swapped_encryption_key() {
        let b = ShieldedBridge::new();
        let (owner_sk, owner) = signer();
        let pk_shield = "cc".repeat(32);
        let honest_pk_encrypt = "dd".repeat(32);
        let sig = sign_register(&owner_sk, &owner, &pk_shield, &honest_pk_encrypt, 0, 1);
        let swapped_pk_encrypt = "ee".repeat(32);
        assert_eq!(
            b.submit_register(&owner, &pk_shield, &swapped_pk_encrypt, 0, &sig, 1, None, None).unwrap_err(),
            ShieldedSubmitError::SignatureInvalid,
            "SECURITY: a signature over one encryption key must not authorize a different one"
        );
    }

    #[test]
    fn register_accepts_a_correctly_signed_request() {
        let b = ShieldedBridge::new();
        let (sk, wallet) = signer();
        let pk_shield = "cc".repeat(32);
        let pk_encrypt = "dd".repeat(32);
        let sig = sign_register(&sk, &wallet, &pk_shield, &pk_encrypt, 0, 1);
        b.submit_register(&wallet, &pk_shield, &pk_encrypt, 0, &sig, 1, None, None)
            .expect("owner-signed, must succeed");
    }

    /// A garbage proof must never reach the queue — that is the DoS guard's whole job.
    #[test]
    fn shielded_send_rejects_a_garbage_proof() {
        let b = ShieldedBridge::new();
        let outs = vec!["11".repeat(32), "22".repeat(32)];
        let err = b
            .submit_shielded_send(
                &"aa".repeat(32), &"bb".repeat(32), &[], &outs,
                sigil_state::shielded::SHIELDED_FEE, vec![0u8; 64], &g3_envelopes(2),
            )
            .unwrap_err();
        assert!(matches!(err, ShieldedSubmitError::ProofRejected(_)), "got {err:?}");
        assert_eq!(b.pending_len(), 0, "nothing may be queued on a bad proof");
    }

    #[test]
    fn wrong_output_arity_is_rejected() {
        let b = ShieldedBridge::new();
        let err = b
            .submit_shielded_send(
                &"aa".repeat(32),
                &"bb".repeat(32),
                &[],
                &["11".repeat(32)],
                sigil_state::shielded::SHIELDED_FEE,
                vec![0u8; 64],
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, ShieldedSubmitError::WrongOutputCount { .. }), "got {err:?}");
    }

    /// A ciphertext count that matches neither "none attached" nor "one per output"
    /// must be refused before the proof is even checked — a DoS-cheap door rejection.
    #[test]
    fn mismatched_ciphertext_count_is_rejected() {
        let b = ShieldedBridge::new();
        let outs = vec!["11".repeat(32), "22".repeat(32)];
        let err = b
            .submit_shielded_send(
                &"aa".repeat(32), &"bb".repeat(32), &[], &outs,
                sigil_state::shielded::SHIELDED_FEE, vec![0u8; 64],
                &[Some("only-one".to_string())],
            )
            .unwrap_err();
        assert_eq!(err, ShieldedSubmitError::WrongCiphertextCount { expected: 2, got: 1 });
    }

    /// THE RELAY HOOK, pinned directly: a successful submit must fire it with the
    /// SAME id it returned and a `ShieldedOp` carrying exactly what a peer's own
    /// `submit_shield` would need to independently re-verify — this is the whole
    /// point of the hook (see `dandelion_relay.rs`'s module docs for why relaying
    /// the ORIGINAL request, not a pre-verified claim, is the safe design).
    #[test]
    fn successful_submit_fires_the_relay_hook_with_a_matching_op() {
        use std::sync::{Arc, Mutex};
        let b = ShieldedBridge::new();
        let captured: Arc<Mutex<Option<([u8; 32], ShieldedOp)>>> = Arc::new(Mutex::new(None));
        let captured2 = Arc::clone(&captured);
        b.set_relay_hook(move |id, op| {
            *captured2.lock().unwrap() = Some((id, op));
        });

        let (sk, from) = signer();
        let cm = "cc".repeat(32);
        let sig = sign_shield(&sk, &from, 5_000, &cm, 2, 9);
        let h = b.submit_shield_with_delivery(&from, 5_000, &cm, 2, &sig, 9, g3_envelope().as_deref()).expect("queued");

        let (id, op) = captured.lock().unwrap().take().expect("relay hook must fire on success");
        assert_eq!(id, h, "hook id must match the submit's own returned id");
        match op {
            ShieldedOp::Shield(r) => {
                assert_eq!(r.from, from);
                assert_eq!(r.amount, 5_000);
                assert_eq!(r.cm, cm);
                assert_eq!(r.fee, 2);
                assert_eq!(r.sig, sig);
                assert_eq!(r.req_nonce, 9);
            }
            other => panic!("expected ShieldedOp::Shield, got {other:?}"),
        }
    }

    /// A failed submit (bad signature) must NOT fire the hook — nothing was
    /// accepted, so there is nothing to relay.
    #[test]
    fn rejected_submit_does_not_fire_the_relay_hook() {
        use std::sync::{Arc, Mutex};
        let b = ShieldedBridge::new();
        let fired = Arc::new(Mutex::new(false));
        let fired2 = Arc::clone(&fired);
        b.set_relay_hook(move |_id, _op| { *fired2.lock().unwrap() = true; });

        let (_sk, from) = signer();
        let _ = b.submit_shield_with_delivery(&from, 5_000, &"dd".repeat(32), 0, "", 1, g3_envelope().as_deref()); // no signature -> refused
        assert!(!*fired.lock().unwrap(), "hook must not fire for a rejected submit");
    }

    /// 2026-09-08 — the shielded half of the v8.0.0 duplicate-payment bug. A tx the
    /// candidate spine already carries must NOT be offered again (offering it produces
    /// a refusal against its own nullifier, and three of those used to evict it as
    /// "rejected" for a payment that had landed). It must be reported `in_flight`, its
    /// attempt count must not move, and it must be offered again the moment the spine
    /// no longer carries it (the carrying candidate was orphaned).
    #[test]
    fn a_tx_the_spine_already_carries_is_not_reoffered_but_stays_pending_in_flight() {
        let b = ShieldedBridge::new();
        let (sk, from) = signer();
        let cm = "cc".repeat(32);
        let sig = sign_shield(&sk, &from, 100, &cm, 0, 1);
        let h = b.submit_shield_with_delivery(&from, 100, &cm, 0, &sig, 1, g3_envelope().as_deref()).expect("queued");

        // Tick 1: not on the spine yet → offered.
        let t1 = b.snapshot_for_mint_excluding(|_| false);
        assert_eq!(t1.len(), 1);
        assert_eq!(b.status(&h), Some(TxOutcome::Pending { attempts: 1, permanent_fails: 0, in_flight: false }));

        // Ticks 2..: the candidate that carried it is on the spine → NOT offered, no
        // attempt counted, reported in flight. Still pending: finality has not happened.
        for _ in 0..3 {
            let t = b.snapshot_for_mint_excluding(|_| true);
            assert!(t.is_empty(), "a carried tx must not be re-offered");
        }
        assert_eq!(b.status(&h), Some(TxOutcome::Pending { attempts: 1, permanent_fails: 0, in_flight: true }));
        assert_eq!(b.pending_len(), 1);

        // The carrying candidate was orphaned: the new spine does not hold it → offered
        // again, exactly the orphan-safety `snapshot_for_mint` was written for.
        let t = b.snapshot_for_mint_excluding(|_| false);
        assert_eq!(t.len(), 1);
        assert_eq!(b.status(&h), Some(TxOutcome::Pending { attempts: 2, permanent_fails: 0, in_flight: false }));

        // Finality on a candidate this node minted → the usual confirm path.
        b.confirm_applied(&[h]);
        assert_eq!(b.status(&h), Some(TxOutcome::Applied));
        assert_eq!(b.pending_len(), 0);
    }

    /// A tx that settled inside a PEER's block never gets a `confirm_applied` (that map
    /// only knows candidates this node minted). `retire_settled` over the settled pool is
    /// what retires it as applied instead of letting it age out as "gave up".
    #[test]
    fn retire_settled_marks_a_peer_carried_tx_applied_and_frees_its_nullifier() {
        let b = ShieldedBridge::new();
        let (sk, from) = signer();
        let cm = "cc".repeat(32);
        let sig = sign_shield(&sk, &from, 100, &cm, 0, 1);
        let h = b.submit_shield_with_delivery(&from, 100, &cm, 0, &sig, 1, g3_envelope().as_deref()).expect("queued");
        assert_eq!(b.retire_settled(|_| false), 0, "not settled yet → nothing retired");
        assert_eq!(b.pending_len(), 1);
        assert_eq!(b.retire_settled(|_| true), 1);
        assert_eq!(b.status(&h), Some(TxOutcome::Applied));
        assert_eq!(b.pending_len(), 0);
        assert_eq!(b.snapshot_for_mint().len(), 0);
    }

    /// `already_in_pool` is the tx's OWN fingerprint against a real pool: a landed
    /// `Shield` is recognised by its commitment; a `ShieldedSend` only when every
    /// nullifier is spent AND every output is held — so a different spend of the same
    /// note (stale client, same-amount index collision) is still offered and still
    /// honestly refused, never silently reported as applied.
    /// 2026-10-01: the predicate must ask about the spend in ITS OWN epoch. Emulates the
    /// chain filing an epoch-1 spend under its scoped key, where the raw (epoch-0) lookup
    /// says "unspent".
    #[test]
    fn a_landed_epoch_one_spend_is_recognised_under_its_scoped_key() {
        let nf = [7u8; 32];
        let outs = [[8u8; 32], [9u8; 32]];
        let scoped = sigil_state::shielded::scoped_nullifier(1, nf);
        let spent_set = [scoped];
        let held = |c: &[u8; 32]| outs.contains(c);
        let raw_lookup = |n: &[u8; 32]| spent_set.contains(n);
        let epoch1_lookup = |n: &[u8; 32]| spent_set.contains(&sigil_state::shielded::scoped_nullifier(1, *n));
        assert!(!spend_fingerprint_present(std::iter::once(&nf), &outs, raw_lookup, held),
            "the OLD raw lookup misses an epoch-1 spend — the bug");
        assert!(spend_fingerprint_present(std::iter::once(&nf), &outs, epoch1_lookup, held),
            "the scoped lookup recognises it");
        assert!(!spend_fingerprint_present(std::iter::once(&nf), &[[8u8; 32], [1u8; 32]], epoch1_lookup, held),
            "a different spend of the same note (other outputs) is still not 'present'");
    }

    #[test]
    fn already_in_pool_is_the_txs_own_fingerprint_not_just_its_nullifier() {
        use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE};
        let (sk, from) = signer();
        let wallet: [u8; 32] = hex::decode(&from).unwrap().try_into().unwrap();
        let mut state = SigilState::default();
        commit_state_transition(
            &mut state,
            &StateTransition {
                at_height: 1,
                mutations: vec![StateMutation::SetBalance { wallet, token: NATIVE, amount: 100_000 }],
            },
            1,
        )
        .unwrap();
        let b = ShieldedBridge::new();
        let cm = "cc".repeat(32);
        let sig = sign_shield(&sk, &from, 100, &cm, 0, 1);
        b.submit_shield_with_delivery(&from, 100, &cm, 0, &sig, 1, g3_envelope().as_deref()).expect("queued");
        let shield_tx = b.snapshot_for_mint()[0].tx.clone();
        assert!(!already_in_pool(state.shielded(), &shield_tx), "before landing: not in the pool");

        let res = sigil_tx::apply_tx_at(&state, &to_signed(shield_tx.clone()), 2).expect("apply");
        commit_state_transition(&mut state, &StateTransition { at_height: 2, mutations: res.mutations }, 2).unwrap();
        assert!(already_in_pool(state.shielded(), &shield_tx), "after landing: recognised by its commitment");

        // A send whose nullifier is unspent and whose outputs are fresh is not "in the pool".
        let send = SigilTx::ShieldedSend {
            anchor: [0u8; 32],
            nullifier: [7u8; 32],
            extra_nullifiers: vec![],
            cm_outs: vec![[8u8; 32], [9u8; 32]],
            fee: sigil_state::shielded::SHIELDED_FEE,
            proof: vec![],
            note_ciphertexts: vec![],
        };
        assert!(!already_in_pool(state.shielded(), &send));
        // Registration is idempotent and never reported as carried.
        assert!(!already_in_pool(
            state.shielded(),
            &SigilTx::RegisterShieldedAddress {
                wallet,
                pk_shield: [1u8; 32],
                pk_encrypt: Some([2u8; 32]),
                pk_sqi: None,
                fee: 0,
            }
        ));
    }

    /// The 13:47 incident: a phone paying a wallet whose registration is still in flight
    /// must be able to learn that it is in flight. And once the pool publishes the keys,
    /// the registration counts as carried (no more re-offering, honest in-flight status).
    #[test]
    fn a_pending_registration_is_visible_and_counts_as_carried_once_published() {
        use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE};
        let b = ShieldedBridge::new();
        let (sk, from) = signer();
        let wallet: [u8; 32] = hex::decode(&from).unwrap().try_into().unwrap();
        assert!(b.pending_registration(&wallet).is_none());

        let pk_shield = "11".repeat(32);
        let pk_enc = "22".repeat(32);
        let msg = format!("sigil-rpc/v1|shield-register|{from}|{pk_shield}|{pk_enc}|0|nonce=1");
        use ed25519_dalek::Signer as _;
        let sig = hex::encode(sk.sign(msg.as_bytes()).to_bytes());
        b.submit_register(&from, &pk_shield, &pk_enc, 0, &sig, 1, None, None).expect("queued");
        assert!(matches!(b.pending_registration(&wallet), Some(TxOutcome::Pending { in_flight: false, .. })));

        let tx = b.snapshot_for_mint()[0].tx.clone();
        let mut state = SigilState::default();
        commit_state_transition(
            &mut state,
            &StateTransition { at_height: 1, mutations: vec![StateMutation::SetBalance { wallet, token: NATIVE, amount: 10 }] },
            1,
        ).unwrap();
        assert!(!already_in_pool(state.shielded(), &tx), "not published yet");
        let res = sigil_tx::apply_tx_at(&state, &to_signed(tx.clone()), 2).expect("apply");
        commit_state_transition(&mut state, &StateTransition { at_height: 2, mutations: res.mutations }, 2).unwrap();
        assert!(already_in_pool(state.shielded(), &tx), "published: carried");

        // The frontier now carries it → not re-offered, reported in flight.
        let fp = state.shielded();
        assert!(b.snapshot_for_mint_excluding(|t| already_in_pool(fp, t)).is_empty());
        assert!(matches!(b.pending_registration(&wallet), Some(TxOutcome::Pending { in_flight: true, .. })));
        // Settled → retired, no longer pending.
        assert_eq!(b.retire_settled(|t| already_in_pool(fp, t)), 1);
        assert!(b.pending_registration(&wallet).is_none());
    }

    /// THE LIVE INCIDENT (2026-08-24/25), reproduced through the REAL retry/re-embed
    /// queue AND the REAL state chokepoint — neither mocked. `snapshot_for_mint` keeps
    /// re-including a not-yet-confirmed `Shield` in every new candidate block until
    /// `confirm_applied` fires, and that only happens once ONE containing block crosses
    /// this chain's real finality depth (~512 blocks / ~30 minutes at the measured live
    /// block rate — see `MAX_AGE`'s doc comment above). A single producer with no
    /// competing candidates mints one new, distinct, immutable block per tick, so dozens
    /// to hundreds of separate blocks can each independently carry the IDENTICAL Shield
    /// payload before the first one is ever confirmed. This test drives exactly that
    /// shape: submit once, call `snapshot_for_mint` twice (two producer ticks before
    /// confirmation), and apply each resulting candidate's tx through the real
    /// `sigil-tx`/`sigil-state` chokepoint at consecutive heights, matching what
    /// `dag_drain_apply` does when it walks the settled spine. Live, this exact
    /// mechanism produced 513 duplicate applications of one 100-unit test deposit before
    /// `MAX_ATTEMPTS`/`MAX_AGE` finally gave up.
    #[test]
    fn the_retry_queue_re_embeds_but_the_chokepoint_refuses_the_replay() {
        use sigil_state::{
            commit_state_transition, CommitError, SigilState, StateMutation, StateTransition,
            NATIVE,
        };

        let bridge = ShieldedBridge::new();
        let (sk, from) = signer();
        let cm = "cc".repeat(32);
        let sig = sign_shield(&sk, &from, 100, &cm, 0, 1);
        bridge.submit_shield_with_delivery(&from, 100, &cm, 0, &sig, 1, g3_envelope().as_deref()).expect("queued");

        // Funded WELL beyond the single 100-unit shield amount — deliberately, so that a
        // second (buggy, pre-fix) application would have had every OTHER opportunity to
        // succeed. If the wallet held only exactly 100, a naive test would "pass" even
        // with the underlying bug still present, because the second application would be
        // refused by ordinary insufficient-balance accounting rather than by the replay
        // guard this test exists to pin — which proves nothing about the actual fix. The
        // live incident's wallet clearly had headroom for this too: it sustained 513
        // consecutive 100-unit debits.
        let wallet_id: [u8; 32] = hex::decode(&from).unwrap().try_into().unwrap();
        let mut state = SigilState::default();
        commit_state_transition(
            &mut state,
            &StateTransition {
                at_height: 1,
                mutations: vec![StateMutation::SetBalance {
                    wallet: wallet_id,
                    token: NATIVE,
                    amount: 100_000,
                }],
            },
            1,
        )
        .expect("fixture funding");

        // TICK 1: the producer's first candidate — mints, embeds the still-pending tx.
        let candidate_1 = bridge.snapshot_for_mint();
        assert_eq!(candidate_1.len(), 1, "one pending tx to embed");
        // TICK 2, BEFORE confirmation: a SECOND, entirely separate candidate — the
        // producer's very next tick, built on top of candidate 1 before candidate 1's
        // own inclusion has come anywhere near crossing final_depth. Same tx, still
        // pending, re-embedded verbatim.
        let candidate_2 = bridge.snapshot_for_mint();
        assert_eq!(candidate_2.len(), 1, "same tx, still not confirmed, embedded again");
        assert_eq!(
            candidate_1[0].tx.hash(),
            candidate_2[0].tx.hash(),
            "both candidates carry the IDENTICAL payload — this is the re-embed, not two \
             distinct user actions"
        );

        let apply_one = |state: &mut SigilState, height: u64, signed: &SignedTx| {
            let result = sigil_tx::apply_tx_at(state, signed, height).expect("precheck+plan");
            commit_state_transition(
                state,
                &StateTransition { at_height: height, mutations: result.mutations },
                height,
            )
        };

        // HEIGHT 2: candidate 1's block lands first — the honest, original application.
        apply_one(&mut state, 2, &candidate_1[0]).expect("the first landing must succeed");
        assert_eq!(state.balance_of(&wallet_id, &NATIVE), 99_900, "debited once");
        assert_eq!(state.shielded().value_locked(), 100, "locked once");
        // notes in BOTH generations (the note lands in the v7 tree on the sigil-g3 family)
        let notes = |s: &SigilState| s.shielded().len() + s.shielded().v7().map_or(0, |e| e.len());
        assert_eq!(notes(&state), 1, "one note");

        // HEIGHT 3: candidate 2's block — a SEPARATE, already-minted, honestly-produced
        // block from the producer's very next tick — lands next. THE BUG: before the
        // fix, this succeeded too — the wallet has ample balance for a second 100-unit
        // debit (that's the whole point of funding it at 100_000), so nothing but the
        // replay guard itself can be what refuses this.
        let err = apply_one(&mut state, 3, &candidate_2[0])
            .expect_err("SECURITY: the retry-queue's re-embedded copy must be refused");
        assert!(
            matches!(
                err,
                CommitError::Shielded(sigil_state::shielded::ShieldedError::DuplicateCommitment(_))
            ),
            "expected DuplicateCommitment, got {err:?}"
        );
        assert_eq!(state.balance_of(&wallet_id, &NATIVE), 99_900, "no phantom second debit");
        assert_eq!(state.shielded().value_locked(), 100, "no phantom locked-value inflation");
        assert_eq!(notes(&state), 1, "no duplicate note");

        // THE OTHER HALF OF THE FIX (`dag.rs`'s `confirm_applied` wiring, 2026-08-24):
        // once candidate 1 actually lands, the queue must stop offering the tx at all —
        // without this, tick 3+ would keep re-embedding it forever (bounded only by
        // MAX_ATTEMPTS/MAX_AGE), manufacturing more doomed candidates for no reason even
        // though the chokepoint fix above already stops them from landing.
        bridge.confirm_applied(&[candidate_1[0].tx.hash()]);
        assert_eq!(bridge.pending_len(), 0, "a confirmed tx must leave the retry queue");
        assert_eq!(bridge.snapshot_for_mint().len(), 0, "nothing left to re-embed");
    }

    /// A pool that knows `anchor` (epoch 0) and has `spent` (already epoch-scoped) in its
    /// spent set. Built through serde because every mutator is crate-private to sigil-state.
    fn pool_with(anchor: [u8; 32], spent: &[[u8; 32]]) -> sigil_state::shielded::ShieldedPool {
        let mut v = serde_json::to_value(sigil_state::shielded::ShieldedPool::default()).unwrap();
        v["anchors"] = serde_json::json!([anchor]);
        v["nullifiers"] = serde_json::json!(spent);
        serde_json::from_value(v).unwrap()
    }

    /// 2026-09-25, reproduced live on sigilgraph.org: a spent note got `ok:true`, a signed
    /// receipt and "✓ sent", then a double-spend reversal ~14 s later. The door must say no
    /// at once, by the same epoch-scoped test the chain applies at commit.
    #[test]
    fn door_refuses_a_nullifier_the_chain_already_spent() {
        let (anchor, nf) = ([9u8; 32], [0xe1u8; 32]);
        let pool = pool_with(anchor, &[sigil_state::shielded::scoped_nullifier(0, nf)]);
        let a = hex::encode(anchor);
        let err = refuse_if_spent(&pool, &a, &[&hex::encode(nf)]).unwrap_err();
        assert_eq!(err, ShieldedSubmitError::AlreadySpent(nf));
        assert!(err.message().contains(&hex::encode(nf)), "names the raw nullifier");
        // The SECOND input of a two-input spend is checked too — not just the first.
        let fresh = hex::encode([3u8; 32]);
        assert_eq!(
            refuse_if_spent(&pool, &a, &[&fresh, &hex::encode(nf)]).unwrap_err(),
            ShieldedSubmitError::AlreadySpent(nf)
        );
        assert_eq!(refuse_if_spent(&pool, &a, &[&fresh]), Ok(()), "an unspent note passes");
    }

    /// The door must never refuse what the chain would accept.
    #[test]
    fn door_defers_to_the_chain_on_unknown_anchors_and_other_epochs() {
        let (anchor, nf) = ([9u8; 32], [0xe1u8; 32]);
        // Spent in epoch 1 only: the same raw tag proven against an epoch-0 anchor is a
        // DIFFERENT note, and the chain accepts it. Unscoped, the door would freeze it.
        let pool = pool_with(anchor, &[sigil_state::shielded::scoped_nullifier(1, nf)]);
        assert_eq!(refuse_if_spent(&pool, &hex::encode(anchor), &[&hex::encode(nf)]), Ok(()));
        // An anchor the pool does not know is the chain's verdict (UnknownAnchor), not ours.
        let pool = pool_with(anchor, &[sigil_state::shielded::scoped_nullifier(0, nf)]);
        assert_eq!(refuse_if_spent(&pool, &hex::encode([7u8; 32]), &[&hex::encode(nf)]), Ok(()));
    }
}

