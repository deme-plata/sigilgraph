//! sigil-handshake — ephemeral per-session handshake (Lock #13).
//!
//! Separates identity from presence. The wallet proves WHO the agent is;
//! the session key proves WHICH live terminal is speaking right now.
//!
//! Three modules:
//!   - [`role`] — SessionRole enum + Capability flags
//!   - [`handshake`] — EphemeralSessionHandshakeV0 wire shape + transcript hash
//!   - [`session`] — derive session-keys from a verified handshake (HKDF stub in P0)
//!
//! The handshake lifecycle:
//!
//!   1. **Generate** — caller creates ephemeral KEM + sig keypairs, picks role +
//!      capabilities + expiry. Calls `EphemeralSessionHandshakeV0::unsigned(...)`.
//!   2. **Sign** — long-term identity (wallet/validator/release-publisher key)
//!      signs the canonical transcript bytes via `handshake.sign_with(...)`.
//!   3. **Broadcast** — sigil-net carries the handshake on whatever topic the
//!      role implies (`/sigil/g0/sessions` for general; topic-specific for
//!      dedicated channels).
//!   4. **Verify** — peer calls `verify_handshake(&hs, &identity_pk)` →
//!      checks expiry, network_id, signature, role/capability allowlist.
//!   5. **Derive** — both sides feed the verified handshake to
//!      `session::derive_keys(&hs)` → (read_key, write_key, mac_key).
//!   6. **Operate** — sigil-net frames all live messages with the session keys.
//!      The long-term wallet key is GONE from the hot path.
//!   7. **Expire** — at `expires_at_ms`, both sides drop session keys. Future
//!      messages on the same `session_id` are rejected.
//!
//! Phase 0 (this commit) ships the wire shape + transcript-hash + stubbed
//! `sign_with` / `verify_handshake` (BLAKE3 placeholder, NOT cryptographic).
//! Phase 1 (`--features real-pq`) wires flux-sqisign for identity signatures.
//! Phase 2 adds X25519+Kyber hybrid KEM via curve25519-dalek + a kyber crate.

pub mod handshake;
pub mod role;
pub mod session;

pub use handshake::{
    transcript_hash, verify_handshake, EphemeralSessionHandshakeV0, HandshakeError,
    SignatureAlgorithm, SessionId, HANDSHAKE_SCHEMA_VERSION,
};
pub use role::{Capability, SessionRole};
pub use session::{derive_keys, SessionKeys};
