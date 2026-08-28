//! Pool-context verification of a ShieldedSend.
//!
//! # Fail-closed by construction (2026-08-20)
//!
//! The PV-0 privacy audit (2026-08-19) found that `verify_zk_stub` returned
//! `Ok(())` unconditionally in the DEFAULT build, so `verify_shielded_send`
//! handed back a `VerifyOk` for any structurally well-formed tx. Double-spend
//! was genuinely enforced (nullifier set), but sum-conservation was not — a
//! malicious producer could have inflated supply while amounts stayed hidden.
//! Nothing was exploitable at the time (this crate had no dependents); the
//! danger was that wiring it into sigil-state's settlement chokepoint as-is
//! would have made a stub look exactly like a passing verification.
//!
//! The rule this module now enforces:
//!
//! > **A `VerifyOk` may only be produced by a path that actually ran a real
//! > proof system. Every other path returns an error.**
//!
//! How that is enforced, rather than merely intended:
//!
//!   - [`VerifyOk`]'s fields are private and its constructor is
//!     module-private AND `#[cfg(feature = "real-zk")]`. In a default build
//!     the crate contains no way to construct a `VerifyOk` at all.
//!   - [`verify_shielded_send`] — the sync entry point — cannot return
//!     `VerifyOk` in ANY cargo configuration. Its ZK step returns
//!     `Result<Infallible, _>`, so "fall through to Ok" is *unrepresentable*
//!     in the type system, not merely unreachable at runtime. A future edit
//!     to `verify_zk_stub` cannot silently re-open the hole.
//!   - [`verify_shielded_send_with_stark`] (`--features real-zk` only) is the
//!     single function in this crate that returns `VerifyOk`, and only after
//!     flux-zk-stark has actually verified a proof bound to this tx.
//!
//! Check order is unchanged, so callers still get the most specific error:
//!   1. `tx.precheck()` — cheap structural checks (sorted, non-empty, ...)
//!   2. Nullifier freshness — none of `input_nullifiers` already spent
//!   3. ZK proof verification — fail-closed unless genuinely verified
//!
//! # What even the `real-zk` path does NOT prove
//!
//! flux-zk-stark's `wallet_privacy_stark` circuit proves a single
//! `(sender, receiver, amount, balance)` transfer with `amount <= balance`.
//! It does NOT prove membership of the spent commitments in
//! `pool.commitments`, and it does NOT prove
//! `sum(input values) == sum(output values) + fee` over this crate's Pedersen
//! commitments — i.e. properties 1-3 of the four that `tx.rs` documents.
//! [`verify_shielded_send_with_stark`] therefore binds what it *can* bind
//! (proof bytes, nullifier, 1-in/1-out arity) and rejects everything else,
//! but it is still NOT sufficient for a settlement chokepoint.
//!
//! Closing that gap is PV-1 work, where sigil-mixer and sigil-shield converge
//! on one note-commitment + nullifier shape. **Do not wire this crate into
//! sigil-state before then.**

use core::convert::Infallible;

use thiserror::Error;

use crate::pool::ShieldedPool;
use crate::tx::{ShieldedSendTxData, TxDataError};

/// Evidence that a ShieldedSend was verified against a pool.
///
/// Constructing one of these is a cryptographic claim, so the fields are
/// private and the constructor is both module-private and gated on
/// `real-zk`. Outside this module the only way to obtain a `VerifyOk` is to
/// be handed one by a function that genuinely ran a proof system — see the
/// module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOk {
    n_inputs: usize,
    n_outputs: usize,
    fee: u128,
}

impl VerifyOk {
    /// Module-private AND `real-zk`-gated on purpose: in a default build this
    /// does not exist, so nothing in the crate can build a `VerifyOk`.
    #[cfg(feature = "real-zk")]
    fn new(tx: &ShieldedSendTxData) -> Self {
        Self {
            n_inputs: tx.input_nullifiers.len(),
            n_outputs: tx.output_commitments.len(),
            fee: tx.fee,
        }
    }

    pub fn n_inputs(&self) -> usize {
        self.n_inputs
    }
    pub fn n_outputs(&self) -> usize {
        self.n_outputs
    }
    pub fn fee(&self) -> u128 {
        self.fee
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum ShieldedSendVerifyError {
    #[error("tx data malformed: {0}")]
    TxData(#[from] TxDataError),
    #[error("double-spend: nullifier {} already in spent set", hex::encode(.0))]
    DoubleSpend([u8; 32]),
    #[error("zk proof verification failed: {0}")]
    ZkVerify(String),
    /// Name kept from the PV-0 audit for traceability. The *semantics* are now
    /// the opposite of what the name suggests: this is the REJECTION returned
    /// when no real proof system is compiled in. A stub is never accepted.
    #[error("fail-closed: no real proof system compiled in — rebuild with `--features real-zk`; a stub must never pass as a verification")]
    ZkStubAccepted,
    /// The proof handed to the verifier is not tied to the tx being verified.
    /// Without this, a valid proof for transaction A could be replayed to
    /// wave through transaction B.
    #[error("zk proof is not bound to this tx: {0}")]
    ZkProofNotBound(&'static str),
}

/// Steps 1 + 2: structural precheck, then nullifier freshness against the
/// pool. Shared by every entry point so the error ordering (and therefore the
/// most-specific-error contract) is identical on all paths.
fn precheck_and_nullifiers(
    tx: &ShieldedSendTxData,
    pool: &ShieldedPool,
) -> Result<(), ShieldedSendVerifyError> {
    tx.precheck()?;

    for n in &tx.input_nullifiers {
        if pool.is_spent(n) {
            return Err(ShieldedSendVerifyError::DoubleSpend(*n.as_bytes()));
        }
    }

    Ok(())
}

/// Verify a ShieldedSend against the current shielded pool.
///
/// **This function never returns `Ok`.** It runs the cheap checks so callers
/// still get the most specific error (malformed tx / double-spend beat the ZK
/// rejection), then fails closed:
///
///   - default build → [`ShieldedSendVerifyError::ZkStubAccepted`]
///   - `--features real-zk` → [`ShieldedSendVerifyError::ZkVerify`], because
///     flux-zk-stark's verifier is `async` and cannot run here. Use
///     [`verify_shielded_send_with_stark`].
///
/// It keeps the sync signature a settlement chokepoint would reach for,
/// precisely so that reaching for it fails loudly instead of silently
/// succeeding.
pub fn verify_shielded_send(
    tx: &ShieldedSendTxData,
    pool: &ShieldedPool,
) -> Result<VerifyOk, ShieldedSendVerifyError> {
    precheck_and_nullifiers(tx, pool)?;

    // `verify_zk_stub` yields `Infallible` on success, so the `?` below can
    // only ever take the error branch and the empty match is the whole
    // remainder of the function. There is no `Ok(...)` tail to fall into.
    match verify_zk_stub(tx)? {}
}

/// Fail-closed ZK step for the sync path. The `Infallible` success type is
/// the point: this function is *incapable* of reporting a passing
/// verification, so no edit short of changing its signature can restore the
/// PV-0 hole.
#[cfg(not(feature = "real-zk"))]
fn verify_zk_stub(_tx: &ShieldedSendTxData) -> Result<Infallible, ShieldedSendVerifyError> {
    Err(ShieldedSendVerifyError::ZkStubAccepted)
}

/// Same fail-closed contract with `real-zk` compiled in: the genuine verifier
/// exists but is `async`, so the sync entry point still cannot run it.
#[cfg(feature = "real-zk")]
fn verify_zk_stub(_tx: &ShieldedSendTxData) -> Result<Infallible, ShieldedSendVerifyError> {
    Err(ShieldedSendVerifyError::ZkVerify(
        "sync verify_shielded_send cannot run flux-zk-stark's async verifier — \
         call verify_shielded_send_with_stark()"
            .to_string(),
    ))
}

/// The only function in this crate that can return a [`VerifyOk`].
///
/// Runs the cheap checks, binds `proof` to `tx`, then asks flux-zk-stark to
/// verify it for real. Read the module-level "What even the `real-zk` path
/// does NOT prove" section before calling this from anything that settles
/// value.
///
/// Binding checks, in order — each one closes a proof-replay route:
///   - `tx.proof.system == "stark"` — the bundle must claim the system we run
///   - `tx.proof.proof_bytes == proof.stark_proof` — the bytes on the wire
///     must BE the proof body handed to the verifier, so a caller cannot
///     verify proof A while the tx carries proof B
///   - exactly one input and one output — flux-zk-stark's circuit covers the
///     1-in/1-out case only; wider txs have no statement covering them
///   - `proof.nullifier == tx.input_nullifiers[0]` — the proof must be about
///     *this* spend
#[cfg(feature = "real-zk")]
pub async fn verify_shielded_send_with_stark(
    tx: &ShieldedSendTxData,
    pool: &ShieldedPool,
    proof: &crate::stark_proofs::StarkProof,
    enable_gpu: bool,
) -> Result<VerifyOk, ShieldedSendVerifyError> {
    precheck_and_nullifiers(tx, pool)?;

    if tx.proof.system != "stark" {
        return Err(ShieldedSendVerifyError::ZkProofNotBound(
            "proof.system must be \"stark\" to be checked by flux-zk-stark",
        ));
    }
    if tx.proof.proof_bytes != proof.stark_proof {
        return Err(ShieldedSendVerifyError::ZkProofNotBound(
            "wire proof_bytes != the STARK proof body handed to the verifier",
        ));
    }
    if tx.input_nullifiers.len() != 1 || tx.output_commitments.len() != 1 {
        return Err(ShieldedSendVerifyError::ZkProofNotBound(
            "flux-zk-stark's wallet_privacy circuit covers 1-input/1-output only",
        ));
    }
    if tx.input_nullifiers[0].as_bytes() != &proof.nullifier {
        return Err(ShieldedSendVerifyError::ZkProofNotBound(
            "proof nullifier != tx input nullifier",
        ));
    }

    match crate::stark_proofs::verify_shielded_send_stark(proof, enable_gpu).await {
        Ok(true) => Ok(VerifyOk::new(tx)),
        Ok(false) => Err(ShieldedSendVerifyError::ZkVerify(
            "flux-zk-stark rejected the proof".to_string(),
        )),
        Err(e) => Err(ShieldedSendVerifyError::ZkVerify(format!(
            "flux-zk-stark verifier error: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::commit;
    use crate::nullifier::derive_nullifier;
    use crate::tx::PrivacyProofBundle;

    fn token() -> [u8; 32] {
        [0u8; 32]
    }

    fn fixture_tx() -> ShieldedSendTxData {
        let c1 = commit(100, &token(), &[1u8; 32]);
        let c2 = commit(50, &token(), &[2u8; 32]);
        let n1 = derive_nullifier(c1, b"alice");
        let n2 = derive_nullifier(c2, b"alice");
        let mut input_nullifiers = vec![n1, n2];
        input_nullifiers.sort();
        let mut output_commitments = vec![
            commit(140, &token(), &[10u8; 32]),
            commit(9, &token(), &[11u8; 32]), // change
        ];
        output_commitments.sort();
        ShieldedSendTxData {
            input_nullifiers,
            output_commitments,
            fee: 1,
            token_hint: token(),
            proof: PrivacyProofBundle {
                system: "groth16".to_string(),
                proof_bytes: vec![1u8; 192],
                commitment_set_root: [0u8; 32],
            },
        }
    }

    /// The PV-0 regression test. This tx used to verify; it must not any more.
    #[test]
    fn well_formed_tx_is_rejected_fail_closed() {
        let pool = ShieldedPool::new();
        let tx = fixture_tx();
        let err = verify_shielded_send(&tx, &pool)
            .expect_err("a stub must never pass as a verification");

        #[cfg(not(feature = "real-zk"))]
        assert!(
            matches!(err, ShieldedSendVerifyError::ZkStubAccepted),
            "default build must reject with ZkStubAccepted, got {err:?}"
        );
        #[cfg(feature = "real-zk")]
        assert!(
            matches!(err, ShieldedSendVerifyError::ZkVerify(_)),
            "real-zk sync path must reject and point at the async verifier, got {err:?}"
        );
    }

    /// Belt-and-braces over the type-level guarantee: no shape of otherwise
    /// valid tx gets a `VerifyOk` out of the sync entry point.
    #[test]
    fn sync_entry_point_never_returns_verify_ok() {
        let pool = ShieldedPool::new();
        for system in ["groth16", "plonk", "stark"] {
            for n_out in 1..3usize {
                let mut tx = fixture_tx();
                tx.proof.system = system.to_string();
                tx.output_commitments.truncate(n_out);
                assert!(
                    verify_shielded_send(&tx, &pool).is_err(),
                    "sync path returned VerifyOk for system={system} n_out={n_out}"
                );
            }
        }
    }

    #[test]
    fn double_spend_detected_when_nullifier_already_in_pool() {
        let mut pool = ShieldedPool::new();
        let tx = fixture_tx();
        // Pre-spend one of the nullifiers.
        pool.spend(tx.input_nullifiers[0]).unwrap();
        let err = verify_shielded_send(&tx, &pool).unwrap_err();
        assert!(matches!(err, ShieldedSendVerifyError::DoubleSpend(_)));
    }

    #[test]
    fn malformed_tx_propagates_precheck_error() {
        let pool = ShieldedPool::new();
        let mut tx = fixture_tx();
        tx.input_nullifiers.clear();
        let err = verify_shielded_send(&tx, &pool).unwrap_err();
        assert!(matches!(
            err,
            ShieldedSendVerifyError::TxData(TxDataError::NoInputs)
        ));
    }

    /// Ordering contract: the cheap, specific errors must still win over the
    /// fail-closed ZK rejection, otherwise every failure would look alike.
    #[test]
    fn specific_errors_still_beat_the_fail_closed_rejection() {
        let mut pool = ShieldedPool::new();
        let tx = fixture_tx();
        pool.spend(tx.input_nullifiers[0]).unwrap();

        let mut malformed = tx.clone();
        malformed.proof.proof_bytes.clear();
        assert!(matches!(
            verify_shielded_send(&malformed, &pool).unwrap_err(),
            ShieldedSendVerifyError::TxData(TxDataError::EmptyProof)
        ));

        assert!(matches!(
            verify_shielded_send(&tx, &pool).unwrap_err(),
            ShieldedSendVerifyError::DoubleSpend(_)
        ));
    }
}
