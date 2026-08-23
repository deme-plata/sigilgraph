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

use serde::{Deserialize, Serialize};
use sigil_tx::{SigilTx, SignedTx};

use crate::send::to_signed;

/// Retry budget per shielded tx, mirroring `send`.
const MAX_ATTEMPTS: u32 = 60;
/// How long a shielded tx may stay pending before it is dropped.
const MAX_AGE: Duration = Duration::from_secs(120);

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
        }
    }
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

/// The pending pool of shielded transactions.
#[derive(Default)]
pub struct ShieldedBridge {
    pending: Mutex<HashMap<[u8; 32], Pending>>,
    /// Nullifiers already represented in the queue, so a duplicate submission does not
    /// occupy two slots and waste two verifications per block.
    queued_nullifiers: Mutex<HashMap<[u8; 32], [u8; 32]>>,
}

impl ShieldedBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a transparent → shielded deposit.
    ///
    /// Wallet-authenticated: the caller must own `from`, and the signature is checked the
    /// same way an ordinary send's is, by the producer's apply path.
    pub fn submit_shield(
        &self,
        from: &str,
        amount: u128,
        cm: &str,
        fee: u128,
    ) -> Result<[u8; 32], ShieldedSubmitError> {
        if amount == 0 {
            return Err(ShieldedSubmitError::ZeroAmount);
        }
        check_denomination(amount)?;
        let from = hex32(from, "from")?;
        let cm = hex32(cm, "cm")?;
        let tx = SigilTx::Shield { from, amount, cm, fee };
        Ok(self.enqueue(tx, None))
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
    pub fn submit_shield_split(
        &self,
        from: &str,
        parts: &[(u128, String)],
        fee: u128,
    ) -> Result<Vec<[u8; 32]>, ShieldedSubmitError> {
        if parts.is_empty() {
            return Err(ShieldedSubmitError::ZeroAmount);
        }
        let from_b = hex32(from, "from")?;
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
    pub fn submit_shielded_send(
        &self,
        anchor: &str,
        nullifier: &str,
        cm_outs: &[String],
        fee: u128,
        proof: Vec<u8>,
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
        self.reject_if_queued(&nf)?;
        self.precheck_proof(&anchor_b, &nf, fee, &outs, &proof)?;

        let tx = SigilTx::ShieldedSend {
            anchor: anchor_b,
            nullifier: nf,
            cm_outs: outs,
            fee,
            proof,
        };
        Ok(self.enqueue(tx, Some(nf)))
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

        let tx = SigilTx::Unshield {
            to: to_b,
            amount,
            anchor: anchor_b,
            nullifier: nf,
            cm_outs: outs,
            proof,
            fee,
        };
        Ok(self.enqueue(tx, Some(nf)))
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
pub struct ShieldRequest {
    pub from: String,
    #[serde(with = "sigil_state::u128_str")]
    pub amount: u128,
    /// `compress2(amount, blinding)` hex — the depositor computes this locally and keeps
    /// the blinding. The server never learns it, which is what makes the note private.
    pub cm: String,
    #[serde(default, with = "sigil_state::u128_str")]
    pub fee: u128,
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

    #[test]
    fn shield_rejects_zero_and_bad_hex() {
        let b = ShieldedBridge::new();
        assert_eq!(
            b.submit_shield("aa", 0, "bb", 0).unwrap_err(),
            ShieldedSubmitError::ZeroAmount
        );
        assert!(matches!(
            b.submit_shield("zz", 1_000, &"11".repeat(32), 0).unwrap_err(),
            ShieldedSubmitError::BadHex("from")
        ));
        assert!(matches!(
            b.submit_shield(&"aa".repeat(32), 1_000, "beef", 0).unwrap_err(),
            ShieldedSubmitError::BadLength { field: "cm", .. }
        ));
    }

    #[test]
    fn shield_enqueues_and_retires() {
        let b = ShieldedBridge::new();
        let h = b.submit_shield(&"aa".repeat(32), 10_000, &"bb".repeat(32), 1).expect("queued");
        assert_eq!(b.pending_len(), 1);
        assert_eq!(b.snapshot_for_mint().len(), 1, "re-embedded until confirmed");
        assert_eq!(b.pending_len(), 1, "snapshot must NOT be destructive");
        b.confirm_applied(&[h]);
        assert_eq!(b.pending_len(), 0);
    }

    /// A garbage proof must never reach the queue — that is the DoS guard's whole job.
    #[test]
    fn shielded_send_rejects_a_garbage_proof() {
        let b = ShieldedBridge::new();
        let outs = vec!["11".repeat(32), "22".repeat(32)];
        let err = b
            .submit_shielded_send(&"aa".repeat(32), &"bb".repeat(32), &outs, sigil_state::shielded::SHIELDED_FEE, vec![0u8; 64])
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
            )
            .unwrap_err();
        assert!(matches!(err, ShieldedSubmitError::WrongOutputCount { .. }), "got {err:?}");
    }
}
