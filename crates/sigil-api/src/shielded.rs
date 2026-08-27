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
use std::sync::Mutex;
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

struct Pending {
    tx: SigilTx,
    attempts: u32,
    first_seen: Instant,
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
}

impl ShieldedSubmitError {
    pub fn message(&self) -> String {
        match self {
            Self::BadHex(f) => format!("{f} must be hex"),
            Self::BadLength { field, expected, got } => {
                format!("{field} must be {expected} bytes, got {got}")
            }
            Self::ZeroAmount => "amount must be > 0".into(),
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
}

impl ShieldedBridge {
    pub fn new() -> Self {
        Self::default()
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
        if amount == 0 {
            return Err(ShieldedSubmitError::ZeroAmount);
        }
        check_denomination(amount)?;
        let from_b = hex32(from, "from")?;
        let cm_b = hex32(cm, "cm")?;
        // Cheap shape validation above, signature/replay checks last — same door-guard
        // ordering the rest of this module already uses (see module docs).
        let msg = format!("sigil-rpc/v1|shield|{from}|{amount}|{cm}|{fee}|nonce={req_nonce}");
        verify_wallet_sig(&from_b, &msg, sig)?;
        self.check_nonce(&from_b, req_nonce)?;
        let tx = SigilTx::Shield { from: from_b, amount, cm: cm_b, fee };
        let id = self.enqueue(tx, None);
        self.fire_relay_hook(id, ShieldedOp::Shield(ShieldRequest {
            from: from.to_string(), amount, cm: cm.to_string(), fee, sig: sig.to_string(), req_nonce,
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
        from: &str,
        parts: &[(u128, String)],
        fee: u128,
        sig: &str,
        req_nonce: u64,
    ) -> Result<Vec<[u8; 32]>, ShieldedSubmitError> {
        if parts.is_empty() {
            return Err(ShieldedSubmitError::ZeroAmount);
        }
        let from_b = hex32(from, "from")?;
        let mut msg = format!("sigil-rpc/v1|shield-split|{from}");
        for (amount, cm) in parts {
            msg.push('|');
            msg.push_str(&amount.to_string());
            msg.push('|');
            msg.push_str(cm);
        }
        msg.push_str(&format!("|{fee}|nonce={req_nonce}"));
        verify_wallet_sig(&from_b, &msg, sig)?;
        self.check_nonce(&from_b, req_nonce)?;
        // Validate EVERY part before enqueuing any, so a bad tail cannot leave a
        // half-shielded balance queued.
        let mut prepared = Vec::with_capacity(parts.len());
        for (amount, cm) in parts {
            if *amount == 0 {
                return Err(ShieldedSubmitError::ZeroAmount);
            }
            check_denomination(*amount)?;
            prepared.push((*amount, hex32(cm, "cm")?));
        }
        Ok(prepared
            .into_iter()
            .enumerate()
            .map(|(i, (amount, cm))| {
                // the fee is charged once, on the first part
                let f = if i == 0 { fee } else { 0 };
                self.enqueue(SigilTx::Shield { from: from_b, amount, cm, fee: f }, None)
            })
            .collect())
    }

    /// Queue a shielded → shielded transfer. No signature: the proof authorizes it.
    ///
    /// `note_ciphertexts` (added 2026-08-24) carries a `sigil_shield::note_cipher`
    /// delivery ciphertext per output, same order as `cm_outs` — pass an empty slice
    /// for "no delivery attached to any output" or one entry per output (any of which
    /// may itself be absent, e.g. for a self-change output the sender already knows).
    /// Not part of the proof's public inputs: it rides on the transaction purely so the
    /// recipient's wallet can discover the payment without an out-of-band channel.
    pub fn submit_shielded_send(
        &self,
        anchor: &str,
        nullifier: &str,
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
        let outs = self.decode_outs(cm_outs)?;
        if !note_ciphertexts.is_empty() && note_ciphertexts.len() != outs.len() {
            return Err(ShieldedSubmitError::WrongCiphertextCount {
                expected: outs.len(),
                got: note_ciphertexts.len(),
            });
        }
        self.reject_if_queued(&nf)?;
        self.precheck_proof(&anchor_b, &nf, fee, &outs, &proof)?;

        let proof_hex = hex::encode(&proof);
        let tx = SigilTx::ShieldedSend {
            anchor: anchor_b,
            nullifier: nf,
            cm_outs: outs,
            fee,
            proof,
            note_ciphertexts: note_ciphertexts.to_vec(),
        };
        let id = self.enqueue(tx, Some(nf));
        self.fire_relay_hook(id, ShieldedOp::ShieldedSend(ShieldedSendRequest {
            anchor: anchor.to_string(), nullifier: nullifier.to_string(), cm_outs: cm_outs.to_vec(),
            fee, proof: proof_hex, note_ciphertexts: note_ciphertexts.to_vec(),
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
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        if amount == 0 {
            return Err(ShieldedSubmitError::ZeroAmount);
        }
        check_denomination(amount)?;
        let to_b = hex32(to, "to")?;
        let anchor_b = hex32(anchor, "anchor")?;
        let nf = hex32(nullifier, "nullifier")?;
        let outs = self.decode_outs(cm_outs)?;
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
        };
        let id = self.enqueue(tx, Some(nf));
        self.fire_relay_hook(id, ShieldedOp::Unshield(UnshieldRequest {
            to: to.to_string(), amount, anchor: anchor.to_string(), nullifier: nullifier.to_string(),
            cm_outs: cm_outs.to_vec(), proof: proof_hex, fee,
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
    fn precheck_proof(
        &self,
        anchor: &[u8; 32],
        nf: &[u8; 32],
        public_value: u128,
        cm_outs: &[[u8; 32]],
        proof: &[u8],
    ) -> Result<(), ShieldedSubmitError> {
        sigil_shield::note_v1::verify_spend_wire(anchor, nf, public_value, cm_outs, proof)
            .map_err(|e| ShieldedSubmitError::ProofRejected(e.to_string()))
    }

    fn enqueue(&self, tx: SigilTx, nf: Option<[u8; 32]>) -> [u8; 32] {
        let hash = tx.hash();
        if let Some(nf) = nf {
            self.queued_nullifiers.lock().unwrap().insert(nf, hash);
        }
        self.pending.lock().unwrap().insert(
            hash,
            Pending { tx, attempts: 0, first_seen: Instant::now() },
        );
        hash
    }

    /// Re-embed every still-pending shielded tx into the next candidate block. Called once
    /// per candidate, NOT once per settled height — same non-destructive contract as
    /// `SendBridge::snapshot_for_mint`.
    pub fn snapshot_for_mint(&self) -> Vec<SignedTx> {
        let mut guard = self.pending.lock().unwrap();
        let mut expired: Vec<[u8; 32]> = Vec::new();
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
            p.attempts += 1;
            out.push(to_signed(p.tx.clone()));
            true
        });
        drop(guard);
        if !expired.is_empty() {
            self.forget_nullifiers(&expired);
        }
        out
    }

    /// Retire landed shielded txs.
    pub fn confirm_applied(&self, hashes: &[[u8; 32]]) {
        if hashes.is_empty() {
            return;
        }
        {
            let mut guard = self.pending.lock().unwrap();
            for h in hashes {
                guard.remove(h);
            }
        }
        self.forget_nullifiers(hashes);
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
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ShieldedSendRequest {
    pub anchor: String,
    pub nullifier: String,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
            b.submit_shield("aa", 0, "bb", 0, "", 1).unwrap_err(),
            ShieldedSubmitError::ZeroAmount
        );
        assert!(matches!(
            b.submit_shield("zz", 1_000, &"11".repeat(32), 0, "", 1).unwrap_err(),
            ShieldedSubmitError::BadHex("from")
        ));
        assert!(matches!(
            b.submit_shield(&"aa".repeat(32), 1_000, "beef", 0, "", 1).unwrap_err(),
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
            b.submit_shield(&owner, 10_000, &"bb".repeat(32), 1, "", 1).unwrap_err(),
            ShieldedSubmitError::SignatureInvalid
        );
        // Signed by someone who is NOT the named wallet — the actual attack this fix
        // closes: knowing `owner`'s address is not the same as owning it.
        let forged = sign_shield(&attacker_sk, &owner, 10_000, &"bb".repeat(32), 1, 1);
        assert_eq!(
            b.submit_shield(&owner, 10_000, &"bb".repeat(32), 1, &forged, 1).unwrap_err(),
            ShieldedSubmitError::SignatureInvalid
        );
    }

    #[test]
    fn shield_enqueues_and_retires() {
        let b = ShieldedBridge::new();
        let (sk, from) = signer();
        let cm = "bb".repeat(32);
        let sig = sign_shield(&sk, &from, 10_000, &cm, 1, 1);
        let h = b.submit_shield(&from, 10_000, &cm, 1, &sig, 1).expect("queued");
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
        b.submit_shield(&from, 10_000, &cm, 1, &sig, 5).expect("first use of nonce 5");
        // Same nonce again, even with a validly re-signed message, must be refused.
        let sig2 = sign_shield(&sk, &from, 10_000, &cm, 1, 5);
        assert_eq!(
            b.submit_shield(&from, 10_000, &cm, 1, &sig2, 5).unwrap_err(),
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
            b.submit_register(&owner, &pk_shield, &pk_encrypt, 0, &forged, 1).unwrap_err(),
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
            b.submit_register(&owner, &pk_shield, &swapped_pk_encrypt, 0, &sig, 1).unwrap_err(),
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
        b.submit_register(&wallet, &pk_shield, &pk_encrypt, 0, &sig, 1)
            .expect("owner-signed, must succeed");
    }

    /// A garbage proof must never reach the queue — that is the DoS guard's whole job.
    #[test]
    fn shielded_send_rejects_a_garbage_proof() {
        let b = ShieldedBridge::new();
        let outs = vec!["11".repeat(32), "22".repeat(32)];
        let err = b
            .submit_shielded_send(
                &"aa".repeat(32), &"bb".repeat(32), &outs,
                sigil_state::shielded::SHIELDED_FEE, vec![0u8; 64], &[],
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
                &"aa".repeat(32), &"bb".repeat(32), &outs,
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
        let h = b.submit_shield(&from, 5_000, &cm, 2, &sig, 9).expect("queued");

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
        let _ = b.submit_shield(&from, 5_000, &"dd".repeat(32), 0, "", 1); // no signature -> refused
        assert!(!*fired.lock().unwrap(), "hook must not fire for a rejected submit");
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
        bridge.submit_shield(&from, 100, &cm, 0, &sig, 1).expect("queued");

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
        assert_eq!(state.shielded().len(), 1, "one note");

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
        assert_eq!(state.shielded().len(), 1, "no duplicate note");

        // THE OTHER HALF OF THE FIX (`dag.rs`'s `confirm_applied` wiring, 2026-08-24):
        // once candidate 1 actually lands, the queue must stop offering the tx at all —
        // without this, tick 3+ would keep re-embedding it forever (bounded only by
        // MAX_ATTEMPTS/MAX_AGE), manufacturing more doomed candidates for no reason even
        // though the chokepoint fix above already stops them from landing.
        bridge.confirm_applied(&[candidate_1[0].tx.hash()]);
        assert_eq!(bridge.pending_len(), 0, "a confirmed tx must leave the retry queue");
        assert_eq!(bridge.snapshot_for_mint().len(), 0, "nothing left to re-embed");
    }
}
