//! CHRONOS-T transport adapter — bridges a [`SimNode`](flux_chronos::SimNode)
//! to real `flux-p2p`.
//!
//! The same `SigilSimNode` that runs under the deterministic in-memory
//! Universe runs over real libp2p through [`RealP2pTransport`]. Sim finds the
//! logic bugs in microseconds; the wire confirms the bytes move. Spec:
//! `flux/docs/flux-chronos-transport-spec.md`.

use flux_p2p::{NetworkManager, SwarmAppEvent};

/// Abstraction the driver loop talks to. Two impls: the in-memory Universe
/// bus (sim) and [`RealP2pTransport`] (wire). The driver code is identical
/// across both — only the Transport differs.
pub trait Transport {
    /// Publish a TAG-prefixed payload to all peers subscribed to `topic`.
    fn publish(&self, topic: &str, payload: Vec<u8>);
    /// Drain every `(topic, payload)` received since the last poll.
    fn poll(&mut self) -> Vec<(String, Vec<u8>)>;
    /// Connected peer count — driver gates block production until ≥1 peer is
    /// up, so the first blocks aren't shouted into an empty room.
    fn peer_count(&self) -> u32;
}

/// Real transport over `flux_p2p::NetworkManager`. Publish → `nm.publish`;
/// poll → filter `nm.drain_events()` for gossipsub messages.
pub struct RealP2pTransport {
    nm: NetworkManager,
}

impl RealP2pTransport {
    /// Wrap a started NetworkManager.
    pub fn new(nm: NetworkManager) -> Self {
        Self { nm }
    }
}

impl Transport for RealP2pTransport {
    fn publish(&self, topic: &str, payload: Vec<u8>) {
        // Best-effort — gossipsub is fire-and-forget. A publish error
        // (e.g. no peers yet) is logged by the driver via peer_count gating.
        let _ = self.nm.publish(topic, payload);
    }

    fn poll(&mut self) -> Vec<(String, Vec<u8>)> {
        self.nm
            .drain_events()
            .into_iter()
            .filter_map(|e| match e {
                SwarmAppEvent::GossipsubMessage { topic, data, .. } => Some((topic, data)),
                _ => None,
            })
            .collect()
    }

    fn peer_count(&self) -> u32 {
        self.nm.peer_count()
    }
}
