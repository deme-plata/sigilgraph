//! SessionRole + Capability — what a session is *for*.
//!
//! A handshake's `role` declares the LIVE FACE the long-term identity is
//! putting forward. Capabilities narrow what that face is allowed to do
//! (least-privilege). The verifier checks both before accepting the session.
//!
//! New roles get added here; verifiers are expected to whitelist the roles
//! they accept (a release-publisher's session shouldn't be honored on the
//! validator-peer topic).

use serde::{Deserialize, Serialize};

/// Lock #13's enumeration of live-coordination surfaces. New roles get
/// appended; never re-purpose a discriminant (the wire format depends on
/// stable variant order via serde's tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionRole {
    /// Browser tip-verifier session (Lock #3 verify-before-sync). The page
    /// authenticates the peer's long-term identity, then operates over a
    /// session key — no replay across browser refreshes.
    BrowserLightClient,
    /// Full-node validator peering session — gossipsub + block sync.
    ValidatorPeer,
    /// MCP-agent session (Claude / GPT / Gemini terminals on the swarm).
    /// Maps `agent_id → session_id` per session.
    McpAgent,
    /// Swarm-worker session — file_claim + swarm_claim + settlement bound.
    SwarmWorker,
    /// DEX trading session — Swap / LpDeposit / LpWithdraw under a single
    /// signing identity; expires per-day for safety.
    DexClient,
    /// Release-publisher session (sigil-updater). Long-term release key
    /// authorises this session to publish announcements until expiry.
    ReleasePublisher,
    /// Email-auth session — out-of-band identity proof for support flows.
    EmailAuth,
    /// Per-conversation payment-channel handshake (designed in session
    /// rocky-updater-2140 as the seed for this whole crate; broadcast as
    /// swarm msg #72; no on-chain tx yet conducted under that token at
    /// memory write time). Memo template binds txs to the session.
    PaymentChannel,
}

impl SessionRole {
    /// Default expiry hint per role — verifiers SHOULD enforce a max
    /// expiry not exceeding this. Operators can set shorter.
    pub fn max_expiry_ms(self) -> u64 {
        use SessionRole::*;
        match self {
            BrowserLightClient => 30 * 60 * 1000,         // 30 min
            ValidatorPeer      => 24 * 60 * 60 * 1000,    // 24 h
            McpAgent           => 24 * 60 * 60 * 1000,    // 24 h
            SwarmWorker        => 8  * 60 * 60 * 1000,    // 8  h
            DexClient          => 24 * 60 * 60 * 1000,    // 24 h
            ReleasePublisher   => 15 * 60 * 1000,         // 15 min — tightest powerful role; release sigs swap binaries network-wide, and the crate's own `release_publisher_has_tightest_expiry` test enforces release <= every other (non-EmailAuth) role
            EmailAuth          => 5  * 60 * 1000,         // 5  min
            PaymentChannel     => 4  * 60 * 60 * 1000,    // 4  h
        }
    }
}

/// Capabilities are role-orthogonal — a McpAgent might also have
/// `SendQug`, a ValidatorPeer typically only has `Gossip + ClaimWork`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    /// Read public chain state via RPC.
    ReadChain,
    /// Subscribe to a gossipsub topic.
    Gossip,
    /// Publish to a gossipsub topic.
    Publish,
    /// Claim swarm tasks.
    ClaimWork,
    /// Complete swarm tasks (triggers settlement).
    CompleteWork,
    /// Send messages via `flux_swarm_message`.
    SendMessage,
    /// Send QUG (or token) from the long-term wallet.
    SendQug,
    /// Submit a swap to the DEX.
    SwapToken,
    /// Publish a release announcement on /sigil/g0/release.
    PublishRelease,
    /// Modify validator-set membership.
    AdjustValidatorSet,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_publisher_has_tightest_expiry() {
        // ReleasePublisher sessions can authorize binary swaps across the
        // whole network — they MUST expire fast.
        let release = SessionRole::ReleasePublisher.max_expiry_ms();
        for other in [
            SessionRole::ValidatorPeer,
            SessionRole::McpAgent,
            SessionRole::SwarmWorker,
            SessionRole::DexClient,
            SessionRole::PaymentChannel,
            SessionRole::BrowserLightClient,
        ] {
            assert!(
                release <= other.max_expiry_ms(),
                "ReleasePublisher expiry must be tightest, but {:?} is shorter",
                other
            );
        }
    }

    #[test]
    fn email_auth_is_short_lived() {
        // EmailAuth is for one-shot proof; 5 min is the upper bound.
        assert_eq!(SessionRole::EmailAuth.max_expiry_ms(), 5 * 60 * 1000);
    }

    #[test]
    fn serde_round_trip_role() {
        for role in [
            SessionRole::BrowserLightClient,
            SessionRole::ValidatorPeer,
            SessionRole::McpAgent,
            SessionRole::SwarmWorker,
            SessionRole::DexClient,
            SessionRole::ReleasePublisher,
            SessionRole::EmailAuth,
            SessionRole::PaymentChannel,
        ] {
            let j = serde_json::to_string(&role).unwrap();
            let back: SessionRole = serde_json::from_str(&j).unwrap();
            assert_eq!(role, back);
        }
    }
}
