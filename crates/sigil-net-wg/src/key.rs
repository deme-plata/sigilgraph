//! WireGuard keypairs.
//!
//! WG uses Curve25519 for the base handshake — same as libp2p, same as
//! `noise` framework. We wrap [`x25519_dalek`] to give a typed surface:
//! `WgPrivateKey`, `WgPublicKey`, `WgPresharedKey`. All three serialize as
//! base64 (the only format `wg(8)` accepts on stdin) and round-trip cleanly.

use serde::{Deserialize, Serialize};

/// 32-byte Curve25519 private key.
#[derive(Clone)]
pub struct WgPrivateKey(x25519_dalek::StaticSecret);

/// 32-byte Curve25519 public key — derivable from a private key, or
/// imported from a peer's published config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WgPublicKey(pub [u8; 32]);

/// 32-byte WireGuard pre-shared key. XORed into the session-key derivation
/// during the handshake — see WireGuard whitepaper §5.4.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WgPresharedKey(pub [u8; 32]);

/// Errors from key parsing / generation.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// Base64 decode failed.
    #[error("base64 decode: {0}")]
    Base64(String),
    /// Key length wasn't 32 bytes after decode.
    #[error("expected 32-byte WireGuard key, got {0} bytes")]
    WrongLength(usize),
}

impl WgPrivateKey {
    /// Generate a fresh keypair from `getrandom`.
    pub fn generate() -> Self {
        Self(x25519_dalek::StaticSecret::random_from_rng(rand_core::OsRng))
    }

    /// Derive the matching public key.
    pub fn public(&self) -> WgPublicKey {
        let p: x25519_dalek::PublicKey = (&self.0).into();
        WgPublicKey(p.to_bytes())
    }

    /// Encode as base64 — the on-wire format `wg(8)` accepts.
    pub fn to_base64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.0.to_bytes())
    }

    /// Decode from base64.
    pub fn from_base64(s: &str) -> Result<Self, KeyError> {
        let bytes = decode_b64_32(s)?;
        Ok(Self(x25519_dalek::StaticSecret::from(bytes)))
    }
}

impl WgPublicKey {
    /// Encode as base64.
    pub fn to_base64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.0)
    }
    /// Decode from base64.
    pub fn from_base64(s: &str) -> Result<Self, KeyError> {
        Ok(Self(decode_b64_32(s)?))
    }
}

impl WgPresharedKey {
    /// Generate a random PSK.
    pub fn generate() -> Self {
        use rand_core::RngCore;
        let mut k = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut k);
        Self(k)
    }
    /// Encode as base64.
    pub fn to_base64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.0)
    }
    /// Decode from base64.
    pub fn from_base64(s: &str) -> Result<Self, KeyError> {
        Ok(Self(decode_b64_32(s)?))
    }
}

// Manual Serialize/Deserialize for the private key (base64 string, never
// raw bytes — minimizes accidental on-wire / on-disk leakage in logs).
impl Serialize for WgPrivateKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_base64())
    }
}
impl<'de> Deserialize<'de> for WgPrivateKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_base64(&s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for WgPublicKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_base64())
    }
}
impl<'de> Deserialize<'de> for WgPublicKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_base64(&s).map_err(serde::de::Error::custom)
    }
}

// Don't print the secret in debug output even by accident.
impl std::fmt::Debug for WgPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WgPrivateKey(<redacted>)")
    }
}
impl std::fmt::Debug for WgPresharedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WgPresharedKey(<redacted>)")
    }
}

fn decode_b64_32(s: &str) -> Result<[u8; 32], KeyError> {
    use base64::Engine;
    let v = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| KeyError::Base64(e.to_string()))?;
    if v.len() != 32 {
        return Err(KeyError::WrongLength(v.len()));
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_and_derive_public() {
        let sk = WgPrivateKey::generate();
        let pk = sk.public();
        // Different generation should produce a different keypair.
        let sk2 = WgPrivateKey::generate();
        assert_ne!(pk, sk2.public());
    }

    #[test]
    fn base64_round_trip_public() {
        let sk = WgPrivateKey::generate();
        let pk = sk.public();
        let b64 = pk.to_base64();
        let pk2 = WgPublicKey::from_base64(&b64).unwrap();
        assert_eq!(pk, pk2);
    }

    #[test]
    fn base64_round_trip_private() {
        let sk = WgPrivateKey::generate();
        let pub_before = sk.public();
        let b64 = sk.to_base64();
        let sk2 = WgPrivateKey::from_base64(&b64).unwrap();
        assert_eq!(pub_before, sk2.public(), "private key must still derive same public");
    }

    #[test]
    fn psk_round_trip() {
        let psk = WgPresharedKey::generate();
        let b64 = psk.to_base64();
        let psk2 = WgPresharedKey::from_base64(&b64).unwrap();
        assert_eq!(psk.0, psk2.0);
    }

    #[test]
    fn wrong_length_rejected() {
        let err = WgPublicKey::from_base64("aGVsbG8=").unwrap_err(); // "hello" = 5 bytes
        assert!(matches!(err, KeyError::WrongLength(5)));
    }

    #[test]
    fn debug_redacts_secret() {
        let sk = WgPrivateKey::generate();
        let s = format!("{:?}", sk);
        assert_eq!(s, "WgPrivateKey(<redacted>)");
    }
}
