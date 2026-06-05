//! session.rs — derive session keys from a verified handshake.
//!
//! ⚠️ Phase 0 (this stub, added to unblock the workspace): keys are
//! `BLAKE3(domain || label || transcript)` — deterministic from the handshake
//! transcript, NOT a KEM-derived shared secret. Real session keys (HKDF-BLAKE3
//! over an X25519+Kyber hybrid shared secret) land in Phase 2. The
//! [`SessionKeys`] shape is stable so sigil-net can frame messages today.

use crate::handshake::EphemeralSessionHandshakeV0;

/// The three directional keys a live session frames messages with.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKeys {
    pub read_key: [u8; 32],
    pub write_key: [u8; 32],
    pub mac_key: [u8; 32],
}

impl std::fmt::Debug for SessionKeys {
    // Never print key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKeys").field("read_key", &"<redacted>").field("write_key", &"<redacted>").field("mac_key", &"<redacted>").finish()
    }
}

/// Derive `(read, write, mac)` keys from a (verified) handshake transcript.
/// Phase-0 BLAKE3-KDF stub — see module note. Both peers derive identical keys
/// from the same handshake.
pub fn derive_keys(hs: &EphemeralSessionHandshakeV0) -> SessionKeys {
    let transcript = hs.transcript_bytes();
    let k = |label: &[u8]| -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-handshake/derive/v0");
        h.update(label);
        h.update(&transcript);
        *h.finalize().as_bytes()
    };
    SessionKeys { read_key: k(b"read"), write_key: k(b"write"), mac_key: k(b"mac") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::{Capability, SessionRole};

    #[test]
    fn derive_is_deterministic_and_distinct() {
        let hs = EphemeralSessionHandshakeV0::unsigned(
            "sigil-g0", vec![1u8; 8], vec![2u8; 8],
            SessionRole::DexClient, vec![Capability::SwapToken], 0, 1000,
        );
        let a = derive_keys(&hs);
        let b = derive_keys(&hs);
        assert_eq!(a, b, "same handshake → same keys (both peers agree)");
        // the three keys must differ from each other (distinct labels).
        assert_ne!(a.read_key, a.write_key);
        assert_ne!(a.write_key, a.mac_key);
        assert_ne!(a.read_key, a.mac_key);
    }
}
