//! Nullifier — owner-derived spend tag for a shielded commitment.
//!
//! Definition: `nullifier = BLAKE3("sigil-nullifier-v0" || commitment || sk)`
//! where `sk` is the owner's spending key. Properties:
//!
//!   - **Unlinkability**: an outsider can't tell which commitment a nullifier
//!     came from without knowing `sk` (preimage resistance).
//!   - **Determinism**: the same (commitment, sk) always derives the same
//!     nullifier — that's what makes it a spend tag. Spending the same
//!     commitment twice would re-derive the same nullifier; the chain rejects
//!     on second appearance.
//!   - **Owner-binding**: only the holder of `sk` can compute the nullifier,
//!     so only they can spend the commitment.
//!
//! Phase 0 uses BLAKE3; Phase 1 will switch to a curve-friendly nullifier
//! derivation matching the Pedersen scheme (e.g. nullifier = blake3(SK · C)
//! where SK·C is scalar multiplication on the curve). The wire shape stays
//! `[u8; 32]`.

use serde::{Deserialize, Serialize};

use crate::commitment::Commitment;

/// Spend tag derived from `(commitment, owner_sk)`. Appears in a
/// `ShieldedSendTxData`'s `input_nullifiers` list to mark commitments as
/// spent. Once a nullifier appears in any block, the chain rejects any
/// future tx that re-derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Nullifier(pub [u8; 32]);

impl Nullifier {
    pub fn as_bytes(&self) -> &[u8; 32] { &self.0 }
    pub fn as_slice(&self) -> &[u8] { &self.0 }
    pub fn to_hex(&self) -> String { hex::encode(self.0) }
    pub fn from_hex(s: &str) -> Result<Self, crate::commitment::CommitmentError> {
        let v = hex::decode(s).map_err(|_| crate::commitment::CommitmentError::BadHex)?;
        if v.len() != 32 {
            return Err(crate::commitment::CommitmentError::WrongLength { got: v.len() });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        Ok(Self(out))
    }
}

impl Serialize for Nullifier {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Nullifier {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s: String = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Derive the nullifier for a (commitment, sk) pair. Only the owner of `sk`
/// can compute this; deterministic, so double-spend = exact-match nullifier.
pub fn derive_nullifier(commitment: Commitment, sk: &[u8]) -> Nullifier {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-nullifier-v0");
    h.update(commitment.as_slice());
    h.update(sk);
    let mut out = [0u8; 32];
    out.copy_from_slice(h.finalize().as_bytes());
    Nullifier(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::commit;

    fn token() -> [u8; 32] { [0u8; 32] }

    #[test]
    fn derive_is_deterministic() {
        let c = commit(100, &token(), &[1u8; 32]);
        let n1 = derive_nullifier(c, b"my-secret-key");
        let n2 = derive_nullifier(c, b"my-secret-key");
        assert_eq!(n1, n2, "same (commitment, sk) must re-derive same nullifier");
    }

    #[test]
    fn different_keys_give_different_nullifiers_for_same_commitment() {
        let c = commit(100, &token(), &[1u8; 32]);
        let alice = derive_nullifier(c, b"alice");
        let bob = derive_nullifier(c, b"bob");
        assert_ne!(alice, bob);
    }

    #[test]
    fn different_commitments_give_different_nullifiers_for_same_key() {
        let c1 = commit(100, &token(), &[1u8; 32]);
        let c2 = commit(200, &token(), &[1u8; 32]);
        let n1 = derive_nullifier(c1, b"alice");
        let n2 = derive_nullifier(c2, b"alice");
        assert_ne!(n1, n2);
    }

    #[test]
    fn serde_json_roundtrip() {
        let c = commit(100, &token(), &[1u8; 32]);
        let n = derive_nullifier(c, b"alice");
        let j = serde_json::to_string(&n).unwrap();
        let back: Nullifier = serde_json::from_str(&j).unwrap();
        assert_eq!(n, back);
        assert!(j.starts_with('"'));
    }
}
