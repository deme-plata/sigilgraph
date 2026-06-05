//! Pool-context verification of a ShieldedSend.
//!
//! Order matters:
//!   1. `tx.precheck()` — cheap structural checks (sorted, non-empty, etc.)
//!   2. Nullifier freshness — none of `input_nullifiers` already in `spent_nullifiers`
//!   3. ZK proof verification — Phase 0 stubbed to `Ok(())` for non-empty proofs;
//!      Phase 1 wires `flux_zk_snark::verify_transaction_privacy_proof()`.
//!
//! Step 3 stub note: in Phase 0 the proof_bytes field is structurally checked
//! (`tx.precheck()` ensures it's non-empty) but the actual ZK statement is
//! NOT cryptographically verified. Validators MUST treat Phase-0 ShieldedSend
//! as "well-formed but trust-the-producer" until P1 lands real verification.
//! This is the same pattern sigil-tx::SignedTx::verify_signature uses today
//! (returns `NotImplemented` from rocky-sigil's Phase-0 implementation).
//!
//! Phase 1 will replace `verify_zk_stub` with a real call into
//! flux-zk-snark's verification path, which proves:
//!   - membership of every spent commitment in `pool.commitments` (at the
//!     anchor root recorded in `proof.commitment_set_root`)
//!   - well-formed range proof for every output
//!   - Σ(input values) == Σ(output values) + fee, all under one token tag
//!   - signer owns spending keys for every input

use thiserror::Error;

use crate::pool::ShieldedPool;
use crate::tx::{ShieldedSendTxData, TxDataError};

#[derive(Debug)]
pub struct VerifyOk {
    pub n_inputs: usize,
    pub n_outputs: usize,
    pub fee: u128,
}

#[derive(Debug, Error, PartialEq)]
pub enum ShieldedSendVerifyError {
    #[error("tx data malformed: {0}")]
    TxData(#[from] TxDataError),
    #[error("double-spend: nullifier {} already in spent set", hex::encode(.0))]
    DoubleSpend([u8; 32]),
    #[error("zk proof verification failed: {0}")]
    ZkVerify(String),
    #[error("phase 0: zk verification stubbed (proof bytes present but not cryptographically verified)")]
    ZkStubAccepted,
}

/// Verify a ShieldedSend against the current shielded pool.
///
/// In Phase 0, returns `Ok(VerifyOk)` for any tx that passes the precheck
/// + nullifier-freshness check, regardless of whether the ZK proof is
/// cryptographically valid. The caller MUST be aware this is trust-the-
/// producer until Phase 1.
pub fn verify_shielded_send(
    tx: &ShieldedSendTxData,
    pool: &ShieldedPool,
) -> Result<VerifyOk, ShieldedSendVerifyError> {
    tx.precheck()?;

    for n in &tx.input_nullifiers {
        if pool.is_spent(n) {
            return Err(ShieldedSendVerifyError::DoubleSpend(*n.as_bytes()));
        }
    }

    // Phase 0 stub. Phase 1: dispatch on tx.proof.system → flux-zk-snark
    // (groth16/plonk) or flux-zk-stark; verify against
    // tx.proof.commitment_set_root snapshot of the pool.
    verify_zk_stub(tx)?;

    Ok(VerifyOk {
        n_inputs: tx.input_nullifiers.len(),
        n_outputs: tx.output_commitments.len(),
        fee: tx.fee,
    })
}

fn verify_zk_stub(_tx: &ShieldedSendTxData) -> Result<(), ShieldedSendVerifyError> {
    // Intentionally trivial in Phase 0. The non-empty check happened in
    // precheck. Returns Ok so the rest of the pipeline (sigil-tx →
    // sigil-state chokepoint) can integrate today.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::commit;
    use crate::nullifier::derive_nullifier;
    use crate::tx::PrivacyProofBundle;

    fn token() -> [u8; 32] { [0u8; 32] }

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

    #[test]
    fn happy_path_verifies_against_empty_pool() {
        let pool = ShieldedPool::new();
        let tx = fixture_tx();
        let ok = verify_shielded_send(&tx, &pool).expect("valid");
        assert_eq!(ok.n_inputs, 2);
        assert_eq!(ok.n_outputs, 2);
        assert_eq!(ok.fee, 1);
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
        assert!(matches!(err, ShieldedSendVerifyError::TxData(TxDataError::NoInputs)));
    }
}
