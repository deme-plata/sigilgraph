//! handshake.rs — `EphemeralSessionHandshakeV0` wire shape + transcript hash +
//! real ed25519 sign/verify (H2).
//!
//! H2 (hardening backlog): `verify_handshake` is now REAL crypto — an ed25519
//! identity signature over the canonical transcript, verified fail-closed. The
//! Phase-0 BLAKE3 placeholder (`sign_with`, `Blake3Stub`) is kept ONLY as a
//! wire-exercise helper and is **rejected by `verify_handshake`**
//! ([`HandshakeError::StubRejected`]) — a stub can never authenticate a peer.
//! PQ identity sigs (SQIsign5/Dilithium5 over the same transcript) remain the
//! Phase-1 upgrade under `--features real-pq`, without changing the wire; until
//! they land, those algs verify as [`HandshakeError::UnsupportedAlgorithm`]
//! (honest fail-closed, not pretend-verify).
//!
//! Channel binding for sync ingress: by convention the requester sets
//! `session_pubkey` to its libp2p peer-id string bytes, and the serving node
//! checks it equals the `PeerId` the request physically arrived from — so an
//! observed handshake replayed from a different peer fails even inside its
//! validity window. (Enforced by the caller — see sigil-node `sync_auth` —
//! because only the transport layer knows the arriving peer.)

use crate::role::{Capability, SessionRole};
use serde::{Deserialize, Serialize};

/// Wire schema version (bump only on a breaking wire change).
pub const HANDSHAKE_SCHEMA_VERSION: u16 = 0;

/// 32-byte session identifier = BLAKE3 of the transcript.
pub type SessionId = [u8; 32];

/// Which signature algorithm authorized the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureAlgorithm {
    /// Phase-0 BLAKE3 placeholder — NOT cryptographic. Rejected by
    /// [`verify_handshake`]; wire-exercise only.
    Blake3Stub,
    /// Phase-1 SQIsign Level 5 (real PQ identity signature).
    SqiSign5,
    /// Dilithium5 alternative.
    Dilithium5,
    /// H2: real ed25519 identity signature over the transcript (the hot-path
    /// scheme; same key shape as the wallet/oauth ed25519 identities).
    Ed25519,
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
    #[error("Blake3Stub is a wire-exercise placeholder, never authenticates — re-sign with a real scheme")]
    StubRejected,
    #[error("identity_pubkey is not a valid key for the declared scheme")]
    BadIdentityKey,
    #[error("identity signature failed cryptographic verification")]
    SignatureInvalid,
    #[error("signature algorithm {0:?} not yet wired for verification (fail-closed)")]
    UnsupportedAlgorithm(SignatureAlgorithm),
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
    /// NOT cryptographic — exercises the wire + flow only. `verify_handshake`
    /// REJECTS stub-signed handshakes ([`HandshakeError::StubRejected`]).
    pub fn sign_with(&mut self, identity_secret: &[u8]) {
        self.sig_alg = SignatureAlgorithm::Blake3Stub;
        let mut h = blake3::Hasher::new();
        h.update(b"sigil-handshake/stub-sign/v0");
        h.update(identity_secret);
        h.update(&self.transcript_bytes());
        self.identity_sig = h.finalize().as_bytes().to_vec();
    }

    /// H2: REAL ed25519 identity signature. Sets `sig_alg = Ed25519` and
    /// `identity_pubkey` to the verifying key derived from `sk` (the transcript
    /// binds both — sign order matters), then signs the canonical transcript.
    pub fn sign_with_ed25519(&mut self, sk: &[u8; 32]) {
        use ed25519_dalek::{Signer, SigningKey};
        let signing = SigningKey::from_bytes(sk);
        self.sig_alg = SignatureAlgorithm::Ed25519;
        self.identity_pubkey = signing.verifying_key().to_bytes().to_vec();
        let msg = self.transcript_bytes();
        self.identity_sig = signing.sign(&msg).to_bytes().to_vec();
    }
}

/// BLAKE3 transcript hash → [`SessionId`].
pub fn transcript_hash(hs: &EphemeralSessionHandshakeV0) -> SessionId {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-handshake/transcript/v0");
    h.update(&hs.transcript_bytes());
    *h.finalize().as_bytes()
}

/// Verify a handshake — H2: REAL cryptographic verification, fail-closed.
///
/// Structural gates (schema / network / expiry / role-max-expiry / role
/// allowlist) run first, then the identity signature is verified against
/// `identity_pubkey` per `sig_alg`:
/// - `Ed25519` — real dalek verification over the canonical transcript.
/// - `Blake3Stub` — ALWAYS rejected ([`HandshakeError::StubRejected`]).
/// - `SqiSign5` / `Dilithium5` — rejected [`HandshakeError::UnsupportedAlgorithm`]
///   until the PQ verify path lands (fail-closed, never pretend-verify).
///
/// Returns the [`SessionId`] on success.
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
    match hs.sig_alg {
        SignatureAlgorithm::Ed25519 => {
            use ed25519_dalek::{Signature, Verifier, VerifyingKey};
            let pk: [u8; 32] = hs.identity_pubkey.as_slice().try_into()
                .map_err(|_| HandshakeError::BadIdentityKey)?;
            let vk = VerifyingKey::from_bytes(&pk)
                .map_err(|_| HandshakeError::BadIdentityKey)?;
            let sig_bytes: [u8; 64] = hs.identity_sig.as_slice().try_into()
                .map_err(|_| HandshakeError::BadSignature)?;
            vk.verify(&hs.transcript_bytes(), &Signature::from_bytes(&sig_bytes))
                .map_err(|_| HandshakeError::SignatureInvalid)?;
        }
        SignatureAlgorithm::Blake3Stub => return Err(HandshakeError::StubRejected),
        alg @ (SignatureAlgorithm::SqiSign5 | SignatureAlgorithm::Dilithium5) => {
            return Err(HandshakeError::UnsupportedAlgorithm(alg));
        }
    }
    Ok(transcript_hash(hs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SK: [u8; 32] = [7u8; 32];

    fn sample(now: u64) -> EphemeralSessionHandshakeV0 {
        let mut hs = EphemeralSessionHandshakeV0::unsigned(
            "sigil-g0",
            Vec::new(), // set by sign_with_ed25519
            vec![2u8; 16],
            SessionRole::McpAgent,
            vec![Capability::ReadChain, Capability::ClaimWork],
            now,
            now + 60 * 60 * 1000, // 1h, under McpAgent's 24h max
        );
        hs.sign_with_ed25519(&SK);
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

    #[test]
    fn stub_signature_never_authenticates() {
        let now = 1_000_000;
        let mut hs = sample(now);
        hs.sign_with(b"long-term-secret"); // downgrade to the Phase-0 stub
        assert!(matches!(
            verify_handshake(&hs, "sigil-g0", now + 1, &[SessionRole::McpAgent]),
            Err(HandshakeError::StubRejected)
        ));
    }

    #[test]
    fn tampered_transcript_fails_signature() {
        let now = 1_000_000;
        // Any signed field flipped after signing must invalidate the signature.
        let mut hs = sample(now);
        hs.session_pubkey = vec![9u8; 16];
        assert!(matches!(
            verify_handshake(&hs, "sigil-g0", now + 1, &[SessionRole::McpAgent]),
            Err(HandshakeError::SignatureInvalid)
        ));
        // A capability grafted on after signing is also caught.
        let mut hs2 = sample(now);
        hs2.capabilities.push(Capability::SendQug);
        assert!(matches!(
            verify_handshake(&hs2, "sigil-g0", now + 1, &[SessionRole::McpAgent]),
            Err(HandshakeError::SignatureInvalid)
        ));
    }

    #[test]
    fn wrong_key_or_malformed_sig_rejected() {
        let now = 1_000_000;
        // Signature swapped for one from a DIFFERENT identity over the same transcript.
        let hs = sample(now);
        let mut other = hs.clone();
        other.sign_with_ed25519(&[8u8; 32]); // re-signs, also swaps identity_pubkey
        let mut forged = hs.clone();
        forged.identity_sig = other.identity_sig.clone(); // sig from the other key
        assert!(matches!(
            verify_handshake(&forged, "sigil-g0", now + 1, &[SessionRole::McpAgent]),
            Err(HandshakeError::SignatureInvalid)
        ));
        // Malformed shapes fail closed.
        let mut short_sig = hs.clone();
        short_sig.identity_sig.truncate(10);
        assert!(matches!(
            verify_handshake(&short_sig, "sigil-g0", now + 1, &[SessionRole::McpAgent]),
            Err(HandshakeError::BadSignature)
        ));
        let mut bad_key = hs.clone();
        bad_key.identity_pubkey = vec![1u8; 7];
        assert!(matches!(
            verify_handshake(&bad_key, "sigil-g0", now + 1, &[SessionRole::McpAgent]),
            Err(HandshakeError::BadIdentityKey)
        ));
    }

    #[test]
    fn pq_algs_fail_closed_until_wired() {
        let now = 1_000_000;
        let mut hs = sample(now);
        hs.sig_alg = SignatureAlgorithm::SqiSign5; // flipping alg also breaks the sig,
                                                   // but the alg gate must fire FIRST
        assert!(matches!(
            verify_handshake(&hs, "sigil-g0", now + 1, &[SessionRole::McpAgent]),
            Err(HandshakeError::UnsupportedAlgorithm(SignatureAlgorithm::SqiSign5))
        ));
    }
}
