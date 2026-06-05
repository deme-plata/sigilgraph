//! Selective Tor egress — "Tor the submission, WireGuard the propagation."
//!
//! Tor is slow (high latency, low bandwidth) and WireGuard already gives the
//! validator mesh confidentiality. So routing the *whole* block-gossip
//! firehose through Tor would be both wasteful and self-defeating. Instead we
//! reserve Tor for the small set of payloads where it actually buys
//! something the rest of the stack can't:
//!
//!   - **TINY** — a few KB at most, so Tor's bandwidth cost is negligible.
//!   - **IDENTITY-REVEALING at the network layer** — the one place an IP↔data
//!     link exists that the chain's own privacy (sigil-mixer) can't hide.
//!   - **OFF-MESH** — talking to a party that isn't in the trusted WG set.
//!
//! The canonical case: a **shielded transaction submission**. On-chain,
//! sigil-mixer hides the amount + parties. But when wallet X submits that tx,
//! the receiving validator sees X's IP — relinking the "anonymous" tx to a
//! real machine. An adversary running validators deanonymizes at the network
//! layer even though the ledger is private. Route JUST that tiny submission
//! over Tor and the two privacy layers finally compose: the chain hides the
//! *contents*, Tor hides the *submitter*. Everything after — the tx mixed
//! into a block, gossiped validator→validator — rides fast WireGuard, because
//! by then there's no IP to leak.
//!
//! This module is the pure decision logic (no Arti dep) + a hard size guard
//! so a bug can never push bulk data through Tor.

use serde::{Deserialize, Serialize};

/// How an outbound payload should leave this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EgressClass {
    /// Block gossip, peer-heights, votes — high bandwidth, among already-
    /// trusted validators. WireGuard: fast + mesh-confidential. NEVER Tor.
    HotMesh,
    /// A shielded-tx SUBMISSION (sigil-mixer `ShieldedSend`). The single place
    /// an IP↔tx link exists; the payload is tiny (a signature + commitment).
    /// Tor breaks the link so chain-privacy + network-privacy compose.
    PrivateSubmit,
    /// Light-client tip-proof query — a browser/mobile asking "is this tip
    /// valid?" without revealing it's a SIGIL user. Tiny. Tor.
    LightQuery,
    /// Off-mesh fetch (oracle price, release metadata) to a non-validator.
    /// Tor for source-anonymity.
    OffMeshFetch,
}

impl EgressClass {
    /// Does this class egress over Tor? (`false` ⇒ the fast WireGuard mesh.)
    pub fn uses_tor(self) -> bool {
        !matches!(self, EgressClass::HotMesh)
    }

    /// Hard upper bound on payload size for a Tor class. Tor is for TINY
    /// payloads; anything larger is either a bug or an attempt to firehose
    /// bulk gossip through Tor — both rejected. `0` ⇒ never eligible.
    pub fn max_tor_bytes(self) -> usize {
        match self {
            EgressClass::HotMesh => 0,                  // never Tor
            EgressClass::PrivateSubmit => 16 * 1024,    // a shielded tx, generously
            EgressClass::LightQuery => 8 * 1024,        // a tip-proof query
            EgressClass::OffMeshFetch => 64 * 1024,     // oracle / metadata blob
        }
    }
}

/// The routing decision for one outbound payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressRoute {
    /// Send over the fast WireGuard mesh (the hot path / default).
    Mesh,
    /// Send over Tor on a per-peer isolated circuit (see
    /// `sigil_net_tor::TorClient::dial_isolated`).
    Tor,
    /// Refused: a Tor-class payload exceeded its size cap. The caller must
    /// split or downgrade — we will NOT push bulk data through Tor.
    RejectedTooBig {
        /// The class that was attempted.
        class: EgressClass,
        /// The payload length offered.
        len: usize,
        /// The class's cap.
        cap: usize,
    },
}

/// Decide how a payload of `payload_len` bytes in `class` should egress.
/// This is the heart of the selective-Tor policy: hot mesh traffic stays on
/// WireGuard; only the tiny privacy-critical classes are eligible for Tor,
/// and only under their size cap.
pub fn route_egress(class: EgressClass, payload_len: usize) -> EgressRoute {
    if !class.uses_tor() {
        return EgressRoute::Mesh;
    }
    let cap = class.max_tor_bytes();
    if payload_len > cap {
        return EgressRoute::RejectedTooBig { class, len: payload_len, cap };
    }
    EgressRoute::Tor
}

/// The libp2p-over-Tor SEND for the tiny privacy classes. Routes via the
/// selective-egress policy, then dials `target` on the per-peer isolated
/// circuit and writes the payload length-prefixed (u32-LE length + bytes).
///
/// REFUSES `HotMesh` (use WireGuard) and any payload over its class cap — so
/// this path structurally cannot carry bulk or hot-path traffic over Tor.
/// Arti-only: there is no Tor without the feature.
#[cfg(feature = "arti")]
pub async fn tor_send(
    client: &crate::TorClient,
    target: &str,
    peer_key: &str,
    class: EgressClass,
    payload: &[u8],
) -> Result<usize, crate::TorClientError> {
    use crate::TorClientError;
    match route_egress(class, payload.len()) {
        EgressRoute::Mesh => {
            return Err(TorClientError::Circuit(format!(
                "{class:?} is hot-path — send over WireGuard, not Tor"
            )))
        }
        EgressRoute::RejectedTooBig { len, cap, .. } => {
            return Err(TorClientError::Circuit(format!(
                "payload {len}B exceeds Tor cap {cap}B for {class:?} — refusing to firehose Tor"
            )))
        }
        EgressRoute::Tor => {}
    }
    // Qualify the circuit key with the egress CLASS so each layer rides its
    // OWN dedicated circuit (a PrivateSubmit and a LightQuery to the same node
    // never share one). dial_isolated folds in the rotation epoch on top.
    let circuit_key = format!("{class:?}::{peer_key}");
    let mut stream = client.dial_isolated(target, &circuit_key).await?;
    let header = (payload.len() as u32).to_le_bytes();
    stream.write(&header).await?;
    let mut sent = 0usize;
    while sent < payload.len() {
        sent += stream.write(&payload[sent..]).await?;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_mesh_never_tor_regardless_of_size() {
        assert!(!EgressClass::HotMesh.uses_tor());
        assert_eq!(route_egress(EgressClass::HotMesh, 1), EgressRoute::Mesh);
        assert_eq!(route_egress(EgressClass::HotMesh, 10_000_000), EgressRoute::Mesh);
    }

    #[test]
    fn tiny_private_submit_goes_tor() {
        assert_eq!(route_egress(EgressClass::PrivateSubmit, 512), EgressRoute::Tor);
        assert_eq!(route_egress(EgressClass::LightQuery, 1024), EgressRoute::Tor);
    }

    #[test]
    fn oversize_tor_payload_is_rejected_not_forwarded() {
        // a 1 MB "shielded tx" is a bug — must NOT silently ride Tor.
        match route_egress(EgressClass::PrivateSubmit, 1_000_000) {
            EgressRoute::RejectedTooBig { class, len, cap } => {
                assert_eq!(class, EgressClass::PrivateSubmit);
                assert_eq!(len, 1_000_000);
                assert_eq!(cap, 16 * 1024);
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn boundary_exactly_at_cap_is_allowed() {
        let cap = EgressClass::LightQuery.max_tor_bytes();
        assert_eq!(route_egress(EgressClass::LightQuery, cap), EgressRoute::Tor);
        assert!(matches!(
            route_egress(EgressClass::LightQuery, cap + 1),
            EgressRoute::RejectedTooBig { .. }
        ));
    }
}
