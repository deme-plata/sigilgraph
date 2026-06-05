//! handshake.rs — `EphemeralSessionHandshakeV0` wire shape + transcript hash +
//! Phase-0 stubbed sign/verify.
//!
//! ⚠️ Phase 0 (this stub, added to UNBLOCK the workspace — lib.rs declared this
//! module but the file was missing → E0583 broke every `fluxc test`): the
//! signature is a **BLAKE3 placeholder, NOT cryptographic**. The wire shape +
//! transcript hash + structural verification (schema / network / expiry / role
//! allowlist) are real and stable. Real PQ identity sigs (SQIsign5/Dilithium5
//! over the transcript) land under `--features real-pq` in Phase 1 — without
//! changing the wire. Owner: replace `sign_with`/`verify_handshake`'s crypto.

use crate::role::{Capability, SessionRole};
use serde::{Deserialize, Serialize};

/// Wire schema version (bump only on a breaking wire change).
pub const HANDSHAKE_SCHEMA_VERSION: u16 = 0;

/// 32-byte session identifier = BLAKE3 of the transcript.
pub type SessionId = [u8; 32];

/// Which signature algorithm authorized the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureAlgorithm {
    /// Phase-0 BLAKE3 placeholder — NOT cryptographic.
    Blake3Stub,
    /// Phase-1 SQIsign Level 5 (real PQ identity signature).
    SqiSign5,
    /// Dilithium5 alternative.
    Dilithium5,
}

/// Why a handshake was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandshakeError {
    #[error("schema version {got} unsupported (this build: {ours})")]
    SchemaMismatch { got: u16, ours: u16 },
    #[error("network_id mismatch: expected {expected}, got {got}")]
    NetworkMismatch { expected: String, got: String },
    #[error("handshake expired at {expires_at_ms} (now {now_ms})")]
    Expired { expires_at_ms: u64, now_ms: u64 },
    #[error("role {0:?} not in the verifier's allowlist")]
    RoleNotAllowed(SessionRole),
    #[error("signature missing or wrong shape (Phase-0 stub expects 32 bytes)")]
    BadSignature,
    #[error("expiry {expires_at_ms} exceeds the role's max ({max_ms})")]
    ExpiryTooLong { expires_at_ms: u64, max_ms: u64 },
}

/// The v0 ephemeral-session handshake. A long-term identity authorizes a
/// short-lived session key for a declared role + capabilities, until expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EphemeralSessionHandshakeV0 {
    pub schema_version: u16,
    pub network_id: String,
    /// Long-term identity pubkey bytes (wallet / validator / release key).
    pub identity_pubkey: Vec<u8>,
    /// Ephemeral session pubkey — the short-lived "live face".
    pub session_pubkey: Vec<u8>,
    pub role: SessionRole,
    pub capabilities: Vec<Capability>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub sig_alg: SignatureAlgorithm,
    /// Identity signature over [`Self::transcript_bytes`] — empty until signed.
    pub identity_sig: Vec<u8>,
}

impl EphemeralSessionHandshakeV0 {
    /// Build an UNSIGNED handshake; caller then [`Self::sign_with`].
    #[allow(clippy::too_many_arguments)]
    pub fn unsigned(
        network_id: impl Into<String>,
        identity_pubkey: Vec<u8>,
        session_pubkey: Vec<u8>,
        role: SessionRole,
        capabilities: Vec<Capability>,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Self {
        Self {
            schema_version: HANDSHAKE_SCHEMA_VERSION,
            network_id: network_id.into(),
            identity_pubkey,
            session_pubkey,
            role,
            capabilities,
            issued_at_ms,
            expires_at_ms,
            sig_alg: SignatureAlgorithm::Blake3Stub,
            identity_sig: Vec::new(),
        }
    }

    /// Canonical bytes that get signed/verified — every field EXCEPT the
    /// signature. Deterministic via serde_json of the signed-field tuple.
    pub fn transcript_bytes(&self) -> Vec<u8> {
        let signed = (
            self.schema_version,
            &self.network_id,
            &self.identity_pubkey,
            &self.session_pubkey,
            &self.role,
            &self.capabilities,
            self.issued_at_ms,
            self.expires_at_ms,
            &self.sig_alg,
        );
        serde_json::to_vec(&signed).unwrap_or_default()
    }

    /// This handshake's [`SessionId`] (BLAKE3 of the transcript).
    pub fn session_id(&self) -> SessionId {
        transcript_hash(self)
    }

    /// Phase-0 STUB sign: `identity_sig = BLAKE3(domain || secret || transcript)`.
    /// NOT cryptographic — exercises the wire + flow only. Real signature lands
    /// under `--features real-pq`.
    pub fn sign_with(&mut self, identity_secret: &[u8]) {
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-handshake/stub-sign/v0");
        h.update(identity_secret);
        h.update(&self.transcript_bytes());
        self.identity_sig = h.finalize().as_bytes().to_vec();
        self.sig_alg = SignatureAlgorithm::Blake3Stub;
    }
}

/// BLAKE3 transcript hash → [`SessionId`].
pub fn transcript_hash(hs: &EphemeralSessionHandshakeV0) -> SessionId {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-handshake/transcript/v0");
    h.update(&hs.transcript_bytes());
    *h.finalize().as_bytes()
}

/// Verify a handshake. Phase-0: structural gates (schema / network / expiry /
/// role-max-expiry / role allowlist) are REAL; the signature check only
/// confirms the stub-sig shape (32 bytes). Real signature verification (SQIsign
/// over the transcript against `identity_pubkey`) is Phase 1. Returns the
/// [`SessionId`] on success.
pub fn verify_handshake(
    hs: &EphemeralSessionHandshakeV0,
    expected_network: &str,
    now_ms: u64,
    allowed_roles: &[SessionRole],
) -> Result<SessionId, HandshakeError> {
    if hs.schema_version != HANDSHAKE_SCHEMA_VERSION {
        return Err(HandshakeError::SchemaMismatch { got: hs.schema_version, ours: HANDSHAKE_SCHEMA_VERSION });
    }
    if hs.network_id != expected_network {
        return Err(HandshakeError::NetworkMismatch { expected: expected_network.to_string(), got: hs.network_id.clone() });
    }
    if now_ms >= hs.expires_at_ms {
        return Err(HandshakeError::Expired { expires_at_ms: hs.expires_at_ms, now_ms });
    }
    let max = hs.role.max_expiry_ms();
    if hs.expires_at_ms.saturating_sub(hs.issued_at_ms) > max {
        return Err(HandshakeError::ExpiryTooLong { expires_at_ms: hs.expires_at_ms, max_ms: max });
    }
    if !allowed_roles.contains(&hs.role) {
        return Err(HandshakeError::RoleNotAllowed(hs.role));
    }
    // Phase-0 stub-sig shape check (real PQ verify in Phase 1).
    if hs.identity_sig.len() != 32 {
        return Err(HandshakeError::BadSignature);
    }
    Ok(transcript_hash(hs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(now: u64) -> EphemeralSessionHandshakeV0 {
        let mut hs = EphemeralSessionHandshakeV0::unsigned(
            "sigil-g0",
            vec![1u8; 16],
            vec![2u8; 16],
            SessionRole::McpAgent,
            vec![Capability::ReadChain, Capability::ClaimWork],
            now,
            now + 60 * 60 * 1000, // 1h, under McpAgent's 24h max
        );
        hs.sign_with(b"long-term-secret");
        hs
    }

    #[test]
    fn sign_then_verify_ok() {
        let now = 1_000_000;
        let hs = sample(now);
        let sid = verify_handshake(&hs, "sigil-g0", now + 1000, &[SessionRole::McpAgent]).expect("verify");
        assert_eq!(sid, hs.session_id());
    }

    #[test]
    fn rejects_expired_wrong_network_and_role() {
        let now = 1_000_000;
        let hs = sample(now);
        assert!(matches!(verify_handshake(&hs, "sigil-g0", hs.expires_at_ms + 1, &[SessionRole::McpAgent]), Err(HandshakeError::Expired { .. })));
        assert!(matches!(verify_handshake(&hs, "mainnet-genesis", now + 1, &[SessionRole::McpAgent]), Err(HandshakeError::NetworkMismatch { .. })));
        assert!(matches!(verify_handshake(&hs, "sigil-g0", now + 1, &[SessionRole::ValidatorPeer]), Err(HandshakeError::RoleNotAllowed(_))));
    }

    #[test]
    fn transcript_is_deterministic() {
        let now = 1_000_000;
        let hs = sample(now);
        assert_eq!(transcript_hash(&hs), transcript_hash(&hs));
    }
}
