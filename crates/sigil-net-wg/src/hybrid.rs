//! Rosenpass-style hybrid PQ pre-shared-key augmentation for WireGuard.
//!
//! ## What this is
//!
//! Plain WireGuard's session-key derivation uses Curve25519 ECDH. If
//! Curve25519 ever falls — say to a CRQC — every captured WG conversation
//! becomes decryptable retroactively. The [Rosenpass](https://rosenpass.eu/)
//! design keeps the WG protocol intact but rotates the optional 32-byte
//! pre-shared key (PSK) every epoch using a Kyber-1024 KEM against the
//! peer's published Kyber pubkey. The new PSK gets XORed into WG's session
//! key derivation, so an attacker now has to break BOTH the WG handshake
//! AND Kyber-1024 in the same epoch — quantum-resistant under the
//! conservative assumption.
//!
//! ## What this isn't (yet)
//!
//! Phase 0 stubs the rotor. The PQ KEM calls return
//! [`HybridPskError::NotImplemented`] until `flux-eternal-cypher` ports
//! from Quillon (it carries the Kyber-1024 KEM SIGIL needs — see the SIGIL
//! skill §"Queued for port from Quillon"). When that lands:
//!
//! 1. [`HybridPsk::generate_kyber_keypair`] returns a real Kyber-1024 keypair.
//! 2. [`HybridPsk::rotate`] does the KEM encapsulation, derives a fresh PSK,
//!    pushes it via the configured [`crate::WgBackend`].
//! 3. The cipher-text gets gossiped to the peer via SIGIL's normal mesh
//!    (topic name TBD when flux-eternal-cypher ports). The peer decapsulates
//!    with their secret key and derives the same PSK.

use crate::key::WgPresharedKey;

/// Stub representation of a Kyber-1024 keypair. Real bytes land with
/// flux-eternal-cypher; the field shapes here track what Kyber-1024 will
/// produce so the surrounding code doesn't have to change.
#[derive(Debug, Clone)]
pub struct KyberKeypair {
    /// 1568-byte Kyber-1024 public key. Currently all-zero placeholder.
    pub public: Vec<u8>,
    /// 3168-byte Kyber-1024 secret key. Currently all-zero placeholder.
    pub secret: Vec<u8>,
}

/// The hybrid rotor. One per SIGIL node — holds the local Kyber secret
/// material and applies fresh PSKs to whichever [`crate::WgBackend`] is
/// driving the kernel interface.
#[derive(Debug)]
pub struct HybridPsk {
    /// Local Kyber keypair (long-lived; persisted to disk by the operator).
    pub kyber: KyberKeypair,
    /// Current epoch counter. Bumped every rotation.
    pub epoch: u64,
    /// Latest derived PSK — `None` until first rotation.
    pub current_psk: Option<WgPresharedKey>,
}

/// Errors from the rotor.
#[derive(Debug, thiserror::Error)]
pub enum HybridPskError {
    /// flux-eternal-cypher hasn't ported the Kyber-1024 KEM yet.
    #[error("kyber-1024 KEM not implemented: {0}")]
    NotImplemented(&'static str),
    /// Wrong-size key.
    #[error("kyber key length: expected {expected}, got {got}")]
    WrongKeyLength {
        /// Bytes expected for this Kyber primitive.
        expected: usize,
        /// Bytes the caller provided.
        got: usize,
    },
}

/// Bytes a Kyber-1024 public key occupies on the wire. From the FIPS-203
/// draft — kept as a constant so callers can pre-size buffers without
/// pulling in the (eventual) PQ crate.
pub const KYBER1024_PUBKEY_BYTES: usize = 1568;

/// Bytes a Kyber-1024 secret key occupies on the wire.
pub const KYBER1024_SECRET_BYTES: usize = 3168;

/// Bytes a Kyber-1024 KEM ciphertext occupies on the wire.
pub const KYBER1024_CIPHERTEXT_BYTES: usize = 1568;

impl HybridPsk {
    /// Construct a new rotor from a previously-saved Kyber keypair.
    /// Validates the byte lengths so a corrupted on-disk keyfile fails loud
    /// instead of silently producing junk PSKs.
    pub fn new(kyber: KyberKeypair) -> Result<Self, HybridPskError> {
        if kyber.public.len() != KYBER1024_PUBKEY_BYTES {
            return Err(HybridPskError::WrongKeyLength {
                expected: KYBER1024_PUBKEY_BYTES, got: kyber.public.len(),
            });
        }
        if kyber.secret.len() != KYBER1024_SECRET_BYTES {
            return Err(HybridPskError::WrongKeyLength {
                expected: KYBER1024_SECRET_BYTES, got: kyber.secret.len(),
            });
        }
        Ok(Self { kyber, epoch: 0, current_psk: None })
    }

    /// Generate a fresh Kyber-1024 keypair. P0 stub — returns
    /// [`HybridPskError::NotImplemented`] until flux-eternal-cypher ports.
    pub fn generate_kyber_keypair() -> Result<KyberKeypair, HybridPskError> {
        Err(HybridPskError::NotImplemented(
            "Kyber-1024 keygen requires flux-eternal-cypher (port from q-eternal-cypher)",
        ))
    }

    /// Encapsulate against `peer_pubkey`, derive a new PSK, bump the
    /// epoch, return the ciphertext for the peer to decapsulate. P0 stub.
    pub fn rotate(&mut self, peer_pubkey: &[u8]) -> Result<Vec<u8>, HybridPskError> {
        if peer_pubkey.len() != KYBER1024_PUBKEY_BYTES {
            return Err(HybridPskError::WrongKeyLength {
                expected: KYBER1024_PUBKEY_BYTES, got: peer_pubkey.len(),
            });
        }
        Err(HybridPskError::NotImplemented(
            "Kyber-1024 encapsulate + PSK derive requires flux-eternal-cypher",
        ))
    }

    /// Receive a peer's encapsulation, decapsulate to derive the same PSK
    /// we derived on rotate. P0 stub.
    pub fn accept(&mut self, _ciphertext: &[u8]) -> Result<(), HybridPskError> {
        Err(HybridPskError::NotImplemented(
            "Kyber-1024 decapsulate requires flux-eternal-cypher",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholder_keypair() -> KyberKeypair {
        KyberKeypair {
            public: vec![0u8; KYBER1024_PUBKEY_BYTES],
            secret: vec![0u8; KYBER1024_SECRET_BYTES],
        }
    }

    #[test]
    fn new_accepts_correctly_sized_keys() {
        let r = HybridPsk::new(placeholder_keypair()).unwrap();
        assert_eq!(r.epoch, 0);
        assert!(r.current_psk.is_none());
    }

    #[test]
    fn new_rejects_wrong_size_pubkey() {
        let mut k = placeholder_keypair();
        k.public = vec![0u8; 32];
        let err = HybridPsk::new(k).unwrap_err();
        assert!(matches!(err, HybridPskError::WrongKeyLength { expected: KYBER1024_PUBKEY_BYTES, got: 32 }));
    }

    #[test]
    fn rotate_stub_returns_not_implemented() {
        let mut r = HybridPsk::new(placeholder_keypair()).unwrap();
        let peer_pub = vec![0u8; KYBER1024_PUBKEY_BYTES];
        assert!(matches!(r.rotate(&peer_pub), Err(HybridPskError::NotImplemented(_))));
    }

    #[test]
    fn rotate_rejects_wrong_size_peer_pubkey_before_stub() {
        let mut r = HybridPsk::new(placeholder_keypair()).unwrap();
        let err = r.rotate(&[0u8; 32]).unwrap_err();
        assert!(matches!(err, HybridPskError::WrongKeyLength { .. }));
    }
}
