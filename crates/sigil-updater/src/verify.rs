//! Verify a `ReleaseAnnouncement` against the binary bytes it describes.
//!
//! Order matters and we fail fast at each step:
//!
//!  1. Announcement format precheck (cheap, no IO).
//!  2. Binary size matches `binary_size_bytes` (catches truncated downloads).
//!  3. BLAKE3(binary) matches `binary_blake3` (catches corruption + swap).
//!  4. SQIsign signature verifies over `signing_bytes()` with `sqisign_pubkey`.
//!
//! Only after all four pass do we report `VerifyOk`. Callers then decide
//! whether to apply (height-gated) or defer.

use crate::announcement::{ReleaseAnnouncement, UpdaterError};

/// What the caller learned after a successful verify. Returned by value so
/// downstream code can log structured fields without re-hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOk {
    pub product: String,
    pub version: String,
    pub binary_blake3_hex: String,
    pub binary_size_bytes: u64,
    pub activation_height: u64,
    pub min_consensus_version: u32,
}

/// Run precheck + signature verification. Does NOT consult the binary bytes;
/// use [`verify_binary_bytes`] for that. Useful for cheap pre-screening of
/// gossipsub announcements before deciding to fetch the binary.
pub fn verify_announcement(a: &ReleaseAnnouncement) -> Result<VerifyOk, UpdaterError> {
    a.precheck()?;
    let msg = a.signing_bytes()?;
    match flux_sqisign::verify(&msg, &a.sqisign_sig, &a.sqisign_pubkey) {
        Ok(true) => Ok(VerifyOk {
            product: a.product.clone(),
            version: a.version.clone(),
            binary_blake3_hex: hex::encode(a.binary_blake3),
            binary_size_bytes: a.binary_size_bytes,
            activation_height: a.activation_height,
            min_consensus_version: a.min_consensus_version,
        }),
        Ok(false) => Err(UpdaterError::SignatureInvalid),
        Err(e) => Err(UpdaterError::Sqisign(e)),
    }
}

/// Verify the binary bytes match the announcement's `binary_size_bytes` and
/// `binary_blake3`. Call after [`verify_announcement`] succeeds and the bytes
/// have been fetched. Constant-time-ish: compares full hash, not prefix.
pub fn verify_binary_bytes(a: &ReleaseAnnouncement, bytes: &[u8]) -> Result<(), UpdaterError> {
    if bytes.len() as u64 != a.binary_size_bytes {
        return Err(UpdaterError::BinarySizeMismatch {
            expected: a.binary_size_bytes,
            actual: bytes.len() as u64,
        });
    }
    let actual = *blake3::hash(bytes).as_bytes();
    if actual != a.binary_blake3 {
        return Err(UpdaterError::BinaryHashMismatch {
            expected_hex: hex::encode(a.binary_blake3),
            actual_hex: hex::encode(actual),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::announcement::ReleaseAnnouncement;

    fn fixture() -> (ReleaseAnnouncement, Vec<u8>) {
        let (sk, pk) = flux_sqisign::keygen();
        let binary = b"fake sigil-node bytes for verify tests".to_vec();
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
        (a, binary)
    }

    #[test]
    fn happy_path_verifies_announcement_and_bytes() {
        let (a, binary) = fixture();
        let ok = verify_announcement(&a).expect("announcement");
        assert_eq!(ok.product, "sigil-node");
        assert_eq!(ok.version, "0.0.2");
        verify_binary_bytes(&a, &binary).expect("binary");
    }

    #[test]
    fn tampered_announcement_field_breaks_signature() {
        let (mut a, _binary) = fixture();
        a.note = "I am the attacker".into();
        assert!(matches!(
            verify_announcement(&a),
            Err(UpdaterError::SignatureInvalid)
        ));
    }

    #[test]
    fn wrong_public_key_breaks_signature() {
        let (mut a, _binary) = fixture();
        let (_sk2, pk2) = flux_sqisign::keygen();
        a.sqisign_pubkey = pk2;
        assert!(matches!(
            verify_announcement(&a),
            Err(UpdaterError::SignatureInvalid) | Err(UpdaterError::Sqisign(_))
        ));
    }

    #[test]
    fn tampered_binary_bytes_break_hash_check() {
        let (a, mut binary) = fixture();
        binary[0] ^= 0x01;
        assert!(matches!(
            verify_binary_bytes(&a, &binary),
            Err(UpdaterError::BinaryHashMismatch { .. })
        ));
    }

    #[test]
    fn truncated_binary_bytes_break_size_check() {
        let (a, binary) = fixture();
        let truncated = &binary[..binary.len() - 1];
        assert!(matches!(
            verify_binary_bytes(&a, truncated),
            Err(UpdaterError::BinarySizeMismatch { .. })
        ));
    }
}
