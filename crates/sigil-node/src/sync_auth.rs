//! H2: verify-before-sync — authenticate sync peers BEFORE serving block ranges.
//!
//! The backlog's H2: `sigil-handshake` was a stub with zero callers; the
//! rr-backfill serve path answered ANY peer. This module is the wiring:
//!
//! - **Requester side** — the node mints ONE ed25519-signed `ValidatorPeer`
//!   handshake at startup ([`NodeIdentity::mint`]) and attaches it to every
//!   outgoing `BackfillReq`. Channel binding: `session_pubkey` = our libp2p
//!   peer-id string bytes, so the serving node can check the handshake against
//!   the peer the request PHYSICALLY arrived from — an observed handshake
//!   replayed by a different peer fails inside its validity window.
//! - **Server side** — [`SyncAuth::admit`] gates the `InboundRequest` serve
//!   path. Sessions are cached per peer until handshake expiry.
//!
//! Rollout safety (mixed-version mesh): enforcement is OFF by default —
//! unauthenticated peers are SERVED but counted + logged (telemetry to size
//! the fleet before the flip). `SIGIL_HANDSHAKE_REQUIRE=1` flips to refusal.
//! The wire is compatible both directions: old servers ignore the unknown
//! `handshake` JSON field; old clients omit it (serde default `None`).
//!
//! The identity key is NOT the wallet key — it lives at
//! `<db_path>/handshake_ed25519.key` (created 0600 on first boot) and only
//! authorizes sync sessions.

use sigil_handshake::{
    verify_handshake, Capability, EphemeralSessionHandshakeV0, SessionRole,
};
use std::collections::HashMap;
use std::path::Path;

/// Session validity we mint for our own outgoing handshake. Half the
/// `ValidatorPeer` 24h max: comfortably under the verifier's ceiling, long
/// enough that a node restarted daily never serves an expired token.
const MINT_VALIDITY_MS: u64 = 12 * 60 * 60 * 1000;

/// Roles a sync server accepts: full nodes and the MCP-agent monitors
/// (sigil-top attaches as `McpAgent` once its client side lands).
const ALLOWED_ROLES: &[SessionRole] = &[SessionRole::ValidatorPeer, SessionRole::McpAgent];

/// Load-or-create the node's handshake identity key (32-byte ed25519 seed).
/// Separate from wallet/producer keys on purpose — leaking it only lets an
/// attacker authenticate as a sync peer, never move funds.
pub fn load_or_create_identity(db_path: &Path) -> [u8; 32] {
    let key_path = db_path.join("handshake_ed25519.key");
    if let Ok(bytes) = std::fs::read(&key_path) {
        if let Ok(sk) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return sk;
        }
        eprintln!("⚠ sync-auth: {} malformed ({} B, want 32) — regenerating", key_path.display(), bytes.len());
    }
    let sk = fresh_seed();
    let _ = std::fs::create_dir_all(db_path);
    if let Err(e) = std::fs::write(&key_path, sk) {
        eprintln!("⚠ sync-auth: could not persist identity key ({e}) — using ephemeral (sessions won't survive restart)");
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
    }
    sk
}

/// 32 random bytes: /dev/urandom on unix; BLAKE3(time ‖ pid ‖ addr) fallback
/// (logged loudly — only reachable on exotic platforms without urandom).
fn fresh_seed() -> [u8; 32] {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut sk = [0u8; 32];
        if f.read_exact(&mut sk).is_ok() {
            return sk;
        }
    }
    eprintln!("⚠ sync-auth: /dev/urandom unavailable — deriving seed from time+pid (weaker)");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let marker = &fresh_seed as *const _ as usize; // ASLR-dependent address entropy
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-sync-auth/fallback-seed/v1");
    h.update(&now.to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    h.update(&marker.to_le_bytes());
    *h.finalize().as_bytes()
}

/// Mint this node's signed sync handshake. `local_peer_id` is the libp2p
/// peer-id string — bound into `session_pubkey` for channel binding.
pub fn mint(sk: &[u8; 32], network_id: &str, local_peer_id: &str, now_ms: u64) -> EphemeralSessionHandshakeV0 {
    let mut hs = EphemeralSessionHandshakeV0::unsigned(
        network_id,
        Vec::new(), // filled by sign_with_ed25519
        local_peer_id.as_bytes().to_vec(),
        SessionRole::ValidatorPeer,
        vec![Capability::ReadChain, Capability::Gossip],
        now_ms,
        now_ms + MINT_VALIDITY_MS,
    );
    hs.sign_with_ed25519(sk);
    hs
}

/// Why a request was (or would be, in log-only mode) refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No handshake attached (legacy peer).
    Missing,
    /// Handshake attached but cryptographically/structurally invalid.
    Invalid,
    /// Valid handshake, but bound to a DIFFERENT peer id than the request
    /// arrived from (replay across peers).
    WrongPeer,
}

/// Server-side gate for the rr-backfill serve path.
pub struct SyncAuth {
    enforce: bool,
    network_id: String,
    /// peer-id string → session expiry (ms). A verified handshake admits the
    /// peer until expiry without re-verifying per request.
    sessions: HashMap<String, u64>,
    pub served_authed: u64,
    pub served_anon: u64,
    pub refused: u64,
}

impl SyncAuth {
    pub fn new(network_id: &str, enforce: bool) -> Self {
        Self {
            enforce,
            network_id: network_id.to_string(),
            sessions: HashMap::new(),
            served_authed: 0,
            served_anon: 0,
            refused: 0,
        }
    }

    /// `SIGIL_HANDSHAKE_REQUIRE=1` → enforce (refuse unauthenticated serves);
    /// default log-only.
    pub fn from_env(network_id: &str) -> Self {
        let enforce = std::env::var("SIGIL_HANDSHAKE_REQUIRE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self::new(network_id, enforce)
    }

    pub fn enforcing(&self) -> bool {
        self.enforce
    }

    /// Gate one inbound backfill request. Returns `Ok(())` to serve, `Err` to
    /// refuse. In log-only mode (default) this ALWAYS returns `Ok`, but keeps
    /// honest counters so the operator can size enforcement.
    pub fn admit(
        &mut self,
        peer_id: &str,
        hs: Option<&EphemeralSessionHandshakeV0>,
        now_ms: u64,
    ) -> Result<(), Refusal> {
        // Cached, unexpired session → serve.
        if let Some(&exp) = self.sessions.get(peer_id) {
            if now_ms < exp {
                self.served_authed += 1;
                return Ok(());
            }
            self.sessions.remove(peer_id);
        }
        let refusal = match hs {
            None => Refusal::Missing,
            Some(hs) => match verify_handshake(hs, &self.network_id, now_ms, ALLOWED_ROLES) {
                Ok(_sid) => {
                    if hs.session_pubkey == peer_id.as_bytes() {
                        self.sessions.insert(peer_id.to_string(), hs.expires_at_ms);
                        self.served_authed += 1;
                        return Ok(());
                    }
                    Refusal::WrongPeer
                }
                Err(_) => Refusal::Invalid,
            },
        };
        if self.enforce {
            self.refused += 1;
            Err(refusal)
        } else {
            self.served_anon += 1;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NET: &str = "sigil-g0";
    const SK: [u8; 32] = [3u8; 32];

    #[test]
    fn minted_handshake_admits_bound_peer_and_caches() {
        let now = 1_000_000;
        let hs = mint(&SK, NET, "12D3KooPeerA", now);
        let mut auth = SyncAuth::new(NET, true);
        assert_eq!(auth.admit("12D3KooPeerA", Some(&hs), now + 1), Ok(()));
        // Cache hit: no handshake needed on the next request from the same peer.
        assert_eq!(auth.admit("12D3KooPeerA", None, now + 2), Ok(()));
        assert_eq!(auth.served_authed, 2);
        // After expiry the cached session dies and a bare request is refused.
        let later = hs.expires_at_ms + 1;
        assert_eq!(auth.admit("12D3KooPeerA", None, later), Err(Refusal::Missing));
    }

    #[test]
    fn replay_from_another_peer_is_refused() {
        let now = 1_000_000;
        let hs = mint(&SK, NET, "12D3KooPeerA", now); // bound to PeerA
        let mut auth = SyncAuth::new(NET, true);
        // PeerB replays PeerA's observed handshake → WrongPeer.
        assert_eq!(auth.admit("12D3KooPeerB", Some(&hs), now + 1), Err(Refusal::WrongPeer));
        assert_eq!(auth.refused, 1);
    }

    #[test]
    fn wrong_network_and_missing_are_refused_when_enforcing() {
        let now = 1_000_000;
        let hs = mint(&SK, "mainnet-genesis", "12D3KooPeerA", now);
        let mut auth = SyncAuth::new(NET, true);
        assert_eq!(auth.admit("12D3KooPeerA", Some(&hs), now + 1), Err(Refusal::Invalid));
        assert_eq!(auth.admit("12D3KooPeerC", None, now + 1), Err(Refusal::Missing));
    }

    #[test]
    fn log_only_mode_serves_everyone_but_counts_honestly() {
        let now = 1_000_000;
        let mut auth = SyncAuth::new(NET, false);
        let good = mint(&SK, NET, "12D3KooPeerA", now);
        let bad = mint(&SK, "wrong-net", "12D3KooPeerB", now);
        assert_eq!(auth.admit("12D3KooPeerA", Some(&good), now + 1), Ok(()));
        assert_eq!(auth.admit("12D3KooPeerB", Some(&bad), now + 1), Ok(())); // served, counted anon
        assert_eq!(auth.admit("12D3KooPeerC", None, now + 1), Ok(()));       // served, counted anon
        assert_eq!(auth.served_authed, 1);
        assert_eq!(auth.served_anon, 2);
        assert_eq!(auth.refused, 0);
    }

    #[test]
    fn identity_key_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("sigil-sync-auth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = load_or_create_identity(&dir);
        let b = load_or_create_identity(&dir);
        assert_eq!(a, b, "second load must return the persisted key");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
