//! Peer chain views — see `sigil_net::peer_view` for the wire type (shared with sigil-top).
//! This module keeps only the flux-p2p record the topology API exposes.

pub use sigil_net::peer_view::{compare, Heartbeat};
use flux_p2p::PeerChainView;

/// The record the topology API exposes per peer.
pub fn to_view(hb: &Heartbeat, seen_ms: u64, tip_matches: Option<bool>, state_root_matches: Option<bool>) -> PeerChainView {
    PeerChainView {
        node: hb.node.clone(),
        height: hb.height,
        tip_hash: hb.tip_hash.map(hex::encode),
        state_root: hb.wallet_state_root.map(hex::encode),
        finalized: hb.finalized,
        ts_ms: hb.ts_ms,
        seen_ms,
        tip_matches_local: tip_matches,
        state_root_matches_local: state_root_matches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_exposes_hex_and_verdicts() {
        let hb = Heartbeat { node: "sigil-g0-epsilon".into(), network: "sigil-g2".into(), ts_ms: 1_788_800_000_000, peers: 5, started: true,
            height: 4_683_730, tip_hash: Some([0xAB; 32]), wallet_state_root: Some([0xCD; 32]), finalized: Some(4_683_218) };
        let v = to_view(&hb, 42, Some(true), Some(false));
        assert_eq!(v.node, "sigil-g0-epsilon");
        assert_eq!(v.height, 4_683_730);
        assert_eq!(v.tip_hash.as_deref(), Some("abababababababababababababababababababababababababababababababab"));
        assert_eq!(v.state_root.as_deref().map(|s| s.len()), Some(64));
        assert_eq!(v.finalized, Some(4_683_218));
        assert_eq!(v.seen_ms, 42);
        assert_eq!((v.tip_matches_local, v.state_root_matches_local), (Some(true), Some(false)));
    }
}
