//! Pedersen commitment wrapper.
//!
//! Real Pedersen: `commitment = value * G + blinding * H` on an elliptic
//! curve, with G and H independent generators. Hides `value` perfectly,
//! binds it computationally.
//!
//! Phase 0 ships an OPAQUE 32-byte wire shape backed by `BLAKE3(value || blinding)`.
//! That's hiding (preimage resistance) and binding (collision resistance) but
//! NOT homomorphic — you can't add two Phase-0 commitments and get
//! `BLAKE3((v1+v2) || (b1+b2))`. Homomorphism is what makes Pedersen useful
//! for sum-conservation proofs without revealing amounts.
//!
//! **Implication**: in P0 the `verify_shielded_send` sum-conservation check
//! relies on the embedded ZK proof (TransactionPrivacyProof) doing the real
//! arithmetic over the secret values. The on-the-wire commitment is just an
//! opaque tag. When P1 swaps in real Pedersen, validators can ALSO verify
//! `sum(input_commitments) == sum(output_commitments) + commit(fee, 0)`
//! homomorphically, as an additional cross-check.
//!
//! The wire shape stays `[u8; 32]` either way, so sigil-tx + sigil-state
//! integrate today and gain the homomorphic cross-check for free in P1.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Opaque 32-byte commitment to (value, blinding). Hex-encoded in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Commitment(pub [u8; 32]);

impl Commitment {
    pub fn as_bytes(&self) -> &[u8; 32] { &self.0 }
    pub fn as_slice(&self) -> &[u8] { &self.0 }
    pub fn to_hex(&self) -> String { hex::encode(self.0) }
    pub fn from_hex(s: &str) -> Result<Self, CommitmentError> {
        let v = hex::decode(s).map_err(|_| CommitmentError::BadHex)?;
        if v.len() != 32 {
            return Err(CommitmentError::WrongLength { got: v.len() });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        Ok(Self(out))
    }
}

impl Serialize for Commitment {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Commitment {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s: String = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Compute a commitment to `(value, token, blinding)`.
///
/// Phase 0: BLAKE3("sigil-commitment-v0" || value || token || blinding).
/// Token is bound into the commitment so a 1-SIGIL commitment can't be
/// pretended to be a 1-USDC commitment (no cross-token mixing).
///
/// Phase 1 will swap to real Pedersen on ark-bn254 G1:
///   C = value_scalar * G + blinding_scalar * H
/// with G and H derived deterministically from a domain-separating tag.
/// The wire shape stays `[u8; 32]` (compressed point); only the bytes change.
pub fn commit(value: u128, token: &[u8; 32], blinding: &[u8; 32]) -> Commitment {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-commitment-v0");
    h.update(&value.to_le_bytes());
    h.update(token);
    h.update(blinding);
    let mut out = [0u8; 32];
    out.copy_from_slice(h.finalize().as_bytes());
    Commitment(out)
}

#[derive(Debug, Error)]
pub enum CommitmentError {
    #[error("commitment hex must decode to 32 bytes (got {got})")]
    WrongLength { got: usize },
    #[error("commitment hex is not valid hex")]
    BadHex,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> [u8; 32] { [0u8; 32] } // native SIGIL
    fn blinding(seed: u8) -> [u8; 32] { [seed; 32] }

    #[test]
    fn commit_is_deterministic() {
        let c1 = commit(100, &token(), &blinding(7));
        let c2 = commit(100, &token(), &blinding(7));
        assert_eq!(c1, c2);
    }

    #[test]
    fn different_values_give_different_commitments() {
        let c1 = commit(100, &token(), &blinding(7));
        let c2 = commit(101, &token(), &blinding(7));
        assert_ne!(c1, c2);
    }

    #[test]
    fn different_blindings_give_different_commitments() {
        let c1 = commit(100, &token(), &blinding(7));
        let c2 = commit(100, &token(), &blinding(8));
        assert_ne!(c1, c2);
    }

    #[test]
    fn different_tokens_give_different_commitments() {
        let mut other_token = [0u8; 32];
        other_token[0] = 1;
        let c1 = commit(100, &token(), &blinding(7));
        let c2 = commit(100, &other_token, &blinding(7));
        assert_ne!(c1, c2, "same value+blinding, different token must commit differently");
    }

    #[test]
    fn hex_roundtrip() {
        let c = commit(100, &token(), &blinding(7));
        let s = c.to_hex();
        let back = Commitment::from_hex(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn serde_json_roundtrip() {
        let c = commit(100, &token(), &blinding(7));
        let j = serde_json::to_string(&c).unwrap();
        let back: Commitment = serde_json::from_str(&j).unwrap();
        assert_eq!(c, back);
        // Should be a string, not an array of integers.
        assert!(j.starts_with('"'));
    }
}
