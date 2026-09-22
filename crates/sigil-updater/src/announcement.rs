//! Release announcement wire schema + signing-bytes derivation.
//!
//! The announcement carries everything a peer needs to fetch, verify, and
//! apply a new sigil-node binary:
//!
//!  - `version`               — semver string for ordering
//!  - `binary_url`            — where to fetch the bytes (HTTP/IPFS/empty)
//!  - `binary_blake3`         — BLAKE3-256 of the exact bytes to install
//!  - `binary_size_bytes`     — sanity-check before downloading
//!  - `proof_blob`            — the fluxc `.proof` bytes emitted alongside
//!                              the binary by `fluxc compile-native
//!                              --provenance`. Embedded so peers don't have
//!                              to fetch separately.
//!  - `sqisign_pubkey`        — release author public key
//!  - `sqisign_sig`           — SQIsign sig over the canonical signing bytes
//!  - `min_consensus_version` — refuse if local consensus version < this
//!  - `activation_height`     — chain height at which nodes swap; gives a
//!                              revocation window via a superseding release
//!  - `timestamp_us`           — author-set wall time at publish
//!  - `note`                  — free-text (changelog line, agent_id, etc.)
//!
//! The signing bytes are the canonical JSON representation with `sqisign_sig`
//! and `sqisign_pubkey` cleared, then serialized with stable key order. This
//! lets verifiers recompute the exact payload that was signed without needing
//! a separate signature-blob format.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Bump if you change the wire schema in a breaking way. Verifiers refuse
/// announcements whose schema_version they don't understand.
pub const ANNOUNCEMENT_SCHEMA_VERSION: u32 = 0;

/// Hard cap on the embedded `.proof` blob. Keeps memory bounded if a malicious
/// publisher tries to ship a 1 GB "proof". A real fluxc proof for an L5
/// SQIsign-signed artifact is ~1-2 KB.
pub const MAX_PROOF_BYTES: usize = 64 * 1024;

/// Hard cap on `binary_size_bytes`. Refuse to even consider absurd sizes.
/// 200 MB is generous for a stripped Rust binary; sigil-node is ~5-15 MB.
pub const MAX_BINARY_BYTES: u64 = 200 * 1024 * 1024;

/// Hard cap on `note`. Just to bound serialized announcement size.
pub const MAX_NOTE_BYTES: usize = 4096;

/// One on-the-wire release announcement. Lives in flux-db, on gossipsub, in
/// HTTP responses — all the same shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAnnouncement {
    pub schema_version: u32,
    pub product: String,
    pub version: String,
    pub binary_url: String,
    #[serde(with = "hex_array_32")]
    pub binary_blake3: [u8; 32],
    pub binary_size_bytes: u64,
    /// Raw bytes of the `.proof` JSON file emitted by `fluxc compile-native
    /// --provenance`. Verifiers can chain-archive this in the next block.
    pub proof_blob: Vec<u8>,
    pub sqisign_pubkey: Vec<u8>,
    pub sqisign_sig: Vec<u8>,
    pub min_consensus_version: u32,
    pub activation_height: u64,
    pub timestamp_us: u64,
    pub note: String,
}

impl ReleaseAnnouncement {
    /// Construct an unsigned announcement ready for `sign()`. Caller supplies
    /// the binary bytes; this function computes the BLAKE3.
    pub fn unsigned(
        product: impl Into<String>,
        version: impl Into<String>,
        binary_url: impl Into<String>,
        binary_bytes: &[u8],
        proof_blob: Vec<u8>,
        sqisign_pubkey: Vec<u8>,
        min_consensus_version: u32,
        activation_height: u64,
        timestamp_us: u64,
        note: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: ANNOUNCEMENT_SCHEMA_VERSION,
            product: product.into(),
            version: version.into(),
            binary_url: binary_url.into(),
            binary_blake3: blake3_of(binary_bytes),
            binary_size_bytes: binary_bytes.len() as u64,
            proof_blob,
            sqisign_pubkey,
            sqisign_sig: Vec::new(),
            min_consensus_version,
            activation_height,
            timestamp_us,
            note: note.into(),
        }
    }

    /// Canonical bytes that the SQIsign signature covers. Zeroes out
    /// `sqisign_sig` first so the sig isn't part of its own input. Uses
    /// `serde_json::to_vec` for determinism — `serde_json::Map` preserves
    /// insertion order, and our struct field order is the canonical order.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, UpdaterError> {
        let mut clone = self.clone();
        clone.sqisign_sig.clear();
        // Serialize via serde_json; deterministic for this struct because we
        // never use HashMaps and field order in derives is source order.
        serde_json::to_vec(&clone).map_err(UpdaterError::Serde)
    }

    /// Sign this announcement in place. Replaces `sqisign_sig` with the result.
    pub fn sign(&mut self, secret_key: &[u8]) -> Result<(), UpdaterError> {
        let msg = self.signing_bytes()?;
        let sig = flux_sqisign::sign(&msg, secret_key, &self.sqisign_pubkey)
            .map_err(UpdaterError::Sqisign)?;
        self.sqisign_sig = sig;
        Ok(())
    }

    /// Cheap format checks before doing expensive work (downloading the binary,
    /// verifying the signature). Returns the first failure.
    pub fn precheck(&self) -> Result<(), UpdaterError> {
        if self.schema_version != ANNOUNCEMENT_SCHEMA_VERSION {
            return Err(UpdaterError::UnsupportedSchemaVersion {
                wanted: ANNOUNCEMENT_SCHEMA_VERSION,
                got: self.schema_version,
            });
        }
        if self.product.is_empty() {
            return Err(UpdaterError::EmptyField("product"));
        }
        if self.version.is_empty() {
            return Err(UpdaterError::EmptyField("version"));
        }
        if self.sqisign_pubkey.is_empty() {
            return Err(UpdaterError::EmptyField("sqisign_pubkey"));
        }
        if self.sqisign_sig.is_empty() {
            return Err(UpdaterError::EmptyField("sqisign_sig"));
        }
        if self.proof_blob.len() > MAX_PROOF_BYTES {
            return Err(UpdaterError::ProofTooLarge {
                size: self.proof_blob.len(),
                max: MAX_PROOF_BYTES,
            });
        }
        if self.binary_size_bytes == 0 {
            return Err(UpdaterError::EmptyField("binary_size_bytes"));
        }
        if self.binary_size_bytes > MAX_BINARY_BYTES {
            return Err(UpdaterError::BinaryTooLarge {
                size: self.binary_size_bytes,
                max: MAX_BINARY_BYTES,
            });
        }
        if self.note.len() > MAX_NOTE_BYTES {
            return Err(UpdaterError::NoteTooLarge {
                size: self.note.len(),
                max: MAX_NOTE_BYTES,
            });
        }
        Ok(())
    }
}

fn blake3_of(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

#[derive(Debug, Error)]
pub enum UpdaterError {
    #[error("unsupported announcement schema_version: wanted {wanted}, got {got}")]
    UnsupportedSchemaVersion { wanted: u32, got: u32 },
    #[error("required field is empty: {0}")]
    EmptyField(&'static str),
    #[error("proof blob too large: {size} > {max}")]
    ProofTooLarge { size: usize, max: usize },
    #[error("binary too large: {size} > {max}")]
    BinaryTooLarge { size: u64, max: u64 },
    #[error("note too large: {size} > {max}")]
    NoteTooLarge { size: usize, max: usize },
    #[error("binary size mismatch: announcement says {expected}, bytes have {actual}")]
    BinarySizeMismatch { expected: u64, actual: u64 },
    #[error("binary BLAKE3 mismatch: announcement says {expected_hex}, bytes hash to {actual_hex}")]
    BinaryHashMismatch { expected_hex: String, actual_hex: String },
    #[error("SQIsign verification failed")]
    SignatureInvalid,
    #[error("release signing key is not on the trusted allowlist: {key_hex}")]
    UntrustedReleaseKey { key_hex: String },
    #[error("SQIsign error: {0}")]
    Sqisign(String),
    #[error("activation height {activation} is not in the future from current height {current}")]
    ActivationInPast { current: u64, activation: u64 },
    #[error("local consensus version {local} is below required minimum {required}")]
    ConsensusTooOld { local: u32, required: u32 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

// Serialize/deserialize a fixed-size [u8; 32] as a hex string. Smaller and
// easier to inspect than a JSON array of integers.
mod hex_array_32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s: String = String::deserialize(d)?;
        let v = hex::decode(&s).map_err(serde::de::Error::custom)?;
        if v.len() != 32 {
            return Err(serde::de::Error::custom(format!(
                "blake3 hex must decode to 32 bytes, got {}",
                v.len()
            )));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (ReleaseAnnouncement, Vec<u8>, Vec<u8>) {
        let (sk, pk) = flux_sqisign::keygen();
        let binary = b"fake sigil-node bytes".to_vec();
        let mut a = ReleaseAnnouncement::unsigned(
            "sigil-node",
            "0.0.2",
            "https://example.org/sigil-node-v0.0.2",
            &binary,
            b"{\"fake\": \"proof\"}".to_vec(),
            pk.clone(),
            1,
            1024,
            42,
            "first test release",
        );
        a.sign(&sk).expect("sign");
        (a, binary, pk)
    }

    #[test]
    fn unsigned_round_trip_through_serde() {
        let (a, _binary, _pk) = fixture();
        let j = serde_json::to_string(&a).expect("ser");
        let back: ReleaseAnnouncement = serde_json::from_str(&j).expect("de");
        assert_eq!(a, back);
    }

    #[test]
    fn signing_bytes_zeroes_signature_field() {
        let (a, _binary, _pk) = fixture();
        let bytes = a.signing_bytes().expect("signing_bytes");
        let text = String::from_utf8_lossy(&bytes);
        // The sqisign_sig in the signing payload must be the empty Vec.
        assert!(text.contains("\"sqisign_sig\":[]"),
            "signing bytes should contain empty sqisign_sig: {}", text);
    }

    #[test]
    fn precheck_rejects_wrong_schema_version() {
        let (mut a, _b, _pk) = fixture();
        a.schema_version = 99;
        assert!(matches!(
            a.precheck(),
            Err(UpdaterError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn precheck_rejects_empty_required_fields() {
        let (mut a, _b, _pk) = fixture();
        a.version.clear();
        assert!(matches!(a.precheck(), Err(UpdaterError::EmptyField("version"))));
    }

    #[test]
    fn precheck_rejects_oversize_proof() {
        let (mut a, _b, _pk) = fixture();
        a.proof_blob = vec![0u8; MAX_PROOF_BYTES + 1];
        assert!(matches!(a.precheck(), Err(UpdaterError::ProofTooLarge { .. })));
    }
}
