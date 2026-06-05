//! ShieldedSend wire shape — replaces SigilTx::Send.
//!
//! What the wire carries:
//!   - `input_nullifiers`: each marks one commitment as spent. The owner had
//!     to know the spending key + the value/blinding to derive these.
//!   - `output_commitments`: new commitments going into the pool. Typically
//!     one per recipient + one change commitment for the sender's leftover.
//!   - `fee`: cleartext u128. Validators need to know it for block reward
//!     calc; revealing fee leaks ROUGHLY how much value was moved (since
//!     fee is usually a percentage), but exact amounts are still hidden.
//!     Phase 2 will switch to commitment fees with a public fee-range proof.
//!   - `token_hint`: cleartext 32-byte tag IFF the entire tx is in a single
//!     token. Empty if multi-token (cross-token swaps). Lets validators
//!     route to the right per-token state without revealing amounts.
//!   - `proof`: the ZK proof bundling everything that needs proving.
//!
//! What the proof asserts:
//!   1. Every input_nullifier was correctly derived from some commitment
//!      that's in the pool (without revealing which one — proof of
//!      membership in `pool.commitments`).
//!   2. Every output_commitment is well-formed: the value it commits to is
//!      in [0, 2^64] (range proof — prevents inflation via wrap-around).
//!   3. Sum-conservation: Σ(input values) == Σ(output values) + fee. No
//!      tokens created or destroyed, all within one token tag.
//!   4. The signer owns the spending keys for each input commitment.
//!
//! Verifier (`verify::verify_shielded_send`) checks the proof + nullifier
//! freshness in the pool. The pool-state changes (insert outputs, spend
//! inputs) happen in sigil-state's chokepoint.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::commitment::Commitment;
use crate::nullifier::Nullifier;

/// The privacy proof bundle. Phase 0 carries
/// `flux_zk_snark::wallet_privacy::TransactionPrivacyProof` bytes verbatim;
/// Phase 1 will add an optional STARK variant for the tip-proof channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyProofBundle {
    /// Which proving system: "groth16" (Phase 0 default) | "plonk" | "stark"
    pub system: String,
    /// Opaque proof bytes — verifier deserializes per `system`.
    pub proof_bytes: Vec<u8>,
    /// Optional anchor: the pool's commitment-set root (Sparse Merkle root)
    /// the proof was generated against. Empty in Phase 0 (when the pool is
    /// a BTreeSet, not an SMT). Lets a verifier ensure the membership-proof
    /// inside `proof_bytes` is against the right pool snapshot at chain
    /// height H.
    pub commitment_set_root: [u8; 32],
}

/// One confidential send. Maps 1:1 onto a future `SigilTx::ShieldedSend`
/// variant — sigil-tx wraps this in a SignedTx with sig_scheme + sig.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShieldedSendTxData {
    /// Nullifiers for the commitments being spent. Order is deterministic
    /// (sorted) to make the tx hash stable across producers.
    pub input_nullifiers: Vec<Nullifier>,
    /// New commitments to insert into the pool. Order is deterministic.
    pub output_commitments: Vec<Commitment>,
    /// Cleartext fee in native SIGIL.
    #[serde(with = "u128_str")]
    pub fee: u128,
    /// Single-token tx: the token-id. Empty (all-zero) if multi-token.
    pub token_hint: [u8; 32],
    /// The ZK proof bundling membership + range + sum-conservation +
    /// ownership.
    pub proof: PrivacyProofBundle,
}

impl ShieldedSendTxData {
    /// Cheap structural checks before doing the expensive ZK verify. Returns
    /// the first failure.
    pub fn precheck(&self) -> Result<(), TxDataError> {
        if self.input_nullifiers.is_empty() {
            return Err(TxDataError::NoInputs);
        }
        if self.output_commitments.is_empty() {
            return Err(TxDataError::NoOutputs);
        }
        // Inputs must be sorted + unique — guards against intra-tx duplicate
        // spend (using the same nullifier twice in the same tx).
        let mut prev: Option<Nullifier> = None;
        for n in &self.input_nullifiers {
            if let Some(p) = prev {
                if *n <= p {
                    return Err(TxDataError::InputsNotSortedOrUnique);
                }
            }
            prev = Some(*n);
        }
        // Same for outputs (intra-tx dup output is a fingerprint issue, not
        // a correctness one, but normalizing helps deterministic tx hash).
        let mut prev: Option<Commitment> = None;
        for c in &self.output_commitments {
            if let Some(p) = prev {
                if *c <= p {
                    return Err(TxDataError::OutputsNotSortedOrUnique);
                }
            }
            prev = Some(*c);
        }
        if self.proof.proof_bytes.is_empty() {
            return Err(TxDataError::EmptyProof);
        }
        if self.proof.system.is_empty() {
            return Err(TxDataError::EmptyProofSystem);
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum TxDataError {
    #[error("no input nullifiers — a ShieldedSend must spend at least one commitment")]
    NoInputs,
    #[error("no output commitments — a ShieldedSend must produce at least one new commitment")]
    NoOutputs,
    #[error("input_nullifiers must be sorted ascending and unique")]
    InputsNotSortedOrUnique,
    #[error("output_commitments must be sorted ascending and unique")]
    OutputsNotSortedOrUnique,
    #[error("proof bytes are empty")]
    EmptyProof,
    #[error("proof system tag is empty")]
    EmptyProofSystem,
}

/// Serde-compatible u128 as a decimal string. Matches sigil-tx's
/// `u128_str` convention (Ethereum / Quillon style).
mod u128_str {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s: String = String::deserialize(d)?;
        s.parse::<u128>().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::commit;
    use crate::nullifier::derive_nullifier;

    fn token() -> [u8; 32] { [0u8; 32] }

    fn fixture(inputs: usize, outputs: usize) -> ShieldedSendTxData {
        let mut input_nullifiers: Vec<_> = (0..inputs as u128)
            .map(|i| {
                let c = commit(100 + i, &token(), &[(i + 1) as u8; 32]);
                derive_nullifier(c, b"alice")
            })
            .collect();
        input_nullifiers.sort();
        let mut output_commitments: Vec<_> = (0..outputs as u128)
            .map(|i| commit(10 + i, &token(), &[(i + 100) as u8; 32]))
            .collect();
        output_commitments.sort();
        ShieldedSendTxData {
            input_nullifiers,
            output_commitments,
            fee: 1,
            token_hint: token(),
            proof: PrivacyProofBundle {
                system: "groth16".to_string(),
                proof_bytes: vec![0u8; 192], // Groth16 typical size
                commitment_set_root: [0u8; 32],
            },
        }
    }

    #[test]
    fn precheck_happy_path() {
        let tx = fixture(2, 3);
        tx.precheck().expect("valid tx");
    }

    #[test]
    fn precheck_rejects_no_inputs() {
        let mut tx = fixture(2, 3);
        tx.input_nullifiers.clear();
        assert_eq!(tx.precheck(), Err(TxDataError::NoInputs));
    }

    #[test]
    fn precheck_rejects_no_outputs() {
        let mut tx = fixture(2, 3);
        tx.output_commitments.clear();
        assert_eq!(tx.precheck(), Err(TxDataError::NoOutputs));
    }

    #[test]
    fn precheck_rejects_unsorted_inputs() {
        let mut tx = fixture(3, 3);
        tx.input_nullifiers.reverse();
        assert_eq!(tx.precheck(), Err(TxDataError::InputsNotSortedOrUnique));
    }

    #[test]
    fn precheck_rejects_duplicate_inputs() {
        let mut tx = fixture(2, 3);
        let first = tx.input_nullifiers[0];
        tx.input_nullifiers[1] = first;
        assert_eq!(tx.precheck(), Err(TxDataError::InputsNotSortedOrUnique));
    }

    #[test]
    fn precheck_rejects_empty_proof() {
        let mut tx = fixture(2, 3);
        tx.proof.proof_bytes.clear();
        assert_eq!(tx.precheck(), Err(TxDataError::EmptyProof));
    }

    #[test]
    fn fee_serializes_as_decimal_string() {
        let tx = fixture(1, 1);
        let j = serde_json::to_string(&tx).unwrap();
        assert!(j.contains("\"fee\":\"1\""), "fee must be string: {}", j);
    }

    #[test]
    fn serde_roundtrip() {
        let tx = fixture(2, 3);
        let j = serde_json::to_string(&tx).unwrap();
        let back: ShieldedSendTxData = serde_json::from_str(&j).unwrap();
        assert_eq!(tx, back);
    }
}
