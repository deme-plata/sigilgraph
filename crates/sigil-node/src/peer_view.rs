//! Peer chain views on the `peer-heights` topic — the cross-node channel the SIGIL
//! K-gauge v2 was missing (2026-09-07).
//!
//! Until now the heartbeat carried `{node, network, ts, peers, started, height}` and the
//! receiver only used `height` to trigger backfill; `/v1/network/topology.peer_heights`
//! was `{}` forever, so the consensus gauge could measure neither finality divergence nor
//! state-root agreement and had to carry 55 % of its channel weight as "unmeasured".
//!
//! The heartbeat now also carries the publisher's **tip hash**, the **wallet state root**
//! committed in that tip header, and its **finalized height**. The receiver looks up its
//! own block at the same height and records a verdict per peer
//! ([`flux_p2p::PeerChainView`]). Everything is tolerant of older nodes: a heartbeat
//! without the new fields decodes with `None`s, and a comparison that is not possible
//! (peer ahead of us, height pruned from RAM) is `None`, never a guessed `false`.
//!
//! Pure module — no network, no chain handle — so every rule here is unit-tested.

use flux_p2p::PeerChainView;
use sigil_header::{BlockHash, Root};

/// One node's statement about its own chain, as published every 5 s.
#[derive(Clone, Debug, PartialEq)]
pub struct Heartbeat {
    pub node: String,
    pub network: String,
    pub ts_ms: u64,
    pub peers: u64,
    pub started: bool,
    /// `chain.height()` — the COUNT of applied blocks; the tip is at index `height − 1`.
    pub height: u64,
    /// Hash of the tip block (`chain.parent_hash()` on the publisher).
    pub tip_hash: Option<BlockHash>,
    /// `wallet_state_root` committed in the tip header.
    pub wallet_state_root: Option<Root>,
    /// Braid `finalized_height()` on the publisher, if it runs the braid.
    pub finalized: Option<u64>,
}

impl Heartbeat {
    /// Wire form: the pre-existing JSON keys (older receivers keep reading `height`) plus
    /// `tip_hash`, `wallet_state_root` (hex) and `finalized`, omitted when unknown.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = serde_json::json!({
            "node":     self.node,
            "network":  self.network,
            "ts":       self.ts_ms,
            "peers":    self.peers,
            "started":  self.started,
            "height":   self.height,
        });
        if let Some(t) = self.tip_hash { v["tip_hash"] = serde_json::Value::String(hex::encode(t)); }
        if let Some(r) = self.wallet_state_root { v["wallet_state_root"] = serde_json::Value::String(hex::encode(r)); }
        if let Some(f) = self.finalized { v["finalized"] = serde_json::Value::from(f); }
        serde_json::to_vec(&v).unwrap_or_default()
    }

    /// Tolerant decode: `height` is the only required key (that is what every node has
    /// published since g0); everything else is optional.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let height = v.get("height")?.as_u64()?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string()).unwrap_or_default();
        Some(Heartbeat {
            node: s("node"),
            network: s("network"),
            ts_ms: v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0),
            peers: v.get("peers").and_then(|x| x.as_u64()).unwrap_or(0),
            started: v.get("started").and_then(|x| x.as_bool()).unwrap_or(false),
            height,
            tip_hash: v.get("tip_hash").and_then(|x| x.as_str()).and_then(hex32),
            wallet_state_root: v.get("wallet_state_root").and_then(|x| x.as_str()).and_then(hex32),
            finalized: v.get("finalized").and_then(|x| x.as_u64()),
        })
    }
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = hex::decode(s.trim_start_matches("0x")).ok()?;
    b.try_into().ok()
}

/// The receiver's verdict on a heartbeat, given ITS OWN block at the publisher's tip
/// height as `(hash, wallet_state_root)` — or `None` when it has no such block (the peer is
/// ahead, or the height has left the in-RAM window). Returns
/// `(tip_matches, state_root_matches)`; each is `None` unless BOTH sides had the datum.
pub fn compare(hb: &Heartbeat, local: Option<(BlockHash, Root)>) -> (Option<bool>, Option<bool>) {
    match local {
        None => (None, None),
        Some((hash, root)) => (hb.tip_hash.map(|t| t == hash), hb.wallet_state_root.map(|w| w == root)),
    }
}

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

    fn hb() -> Heartbeat {
        Heartbeat { node: "sigil-g0-epsilon".into(), network: "sigil-g2".into(), ts_ms: 1_788_800_000_000, peers: 5, started: true,
            height: 4_683_730, tip_hash: Some([0xAB; 32]), wallet_state_root: Some([0xCD; 32]), finalized: Some(4_683_218) }
    }

    #[test]
    fn roundtrip_carries_every_field() {
        let h = hb();
        let back = Heartbeat::decode(&h.encode()).unwrap();
        assert_eq!(back, h);
        let v: serde_json::Value = serde_json::from_slice(&h.encode()).unwrap();
        assert_eq!(v["tip_hash"].as_str().unwrap().len(), 64);
        assert_eq!(v["height"].as_u64(), Some(4_683_730), "old receivers keep reading `height`");
    }

    #[test]
    fn a_pre_v2_heartbeat_still_decodes_with_unknowns() {
        let legacy = br#"{"node":"happysrv","network":"sigil-g2","ts":1,"peers":2,"started":true,"height":820560}"#;
        let h = Heartbeat::decode(legacy).unwrap();
        assert_eq!(h.height, 820_560);
        assert_eq!(h.tip_hash, None);
        assert_eq!(h.wallet_state_root, None);
        assert_eq!(h.finalized, None);
        assert!(Heartbeat::decode(b"{\"node\":\"x\"}").is_none(), "no height → not a heartbeat");
        assert!(Heartbeat::decode(b"not json").is_none());
    }

    #[test]
    fn compare_is_none_unless_both_sides_have_the_datum() {
        let h = hb();
        assert_eq!(compare(&h, None), (None, None), "peer ahead / pruned → unknown, never false");
        assert_eq!(compare(&h, Some(([0xAB; 32], [0xCD; 32]))), (Some(true), Some(true)));
        assert_eq!(compare(&h, Some(([0x00; 32], [0xCD; 32]))), (Some(false), Some(true)), "same balances, different tip");
        assert_eq!(compare(&h, Some(([0xAB; 32], [0x00; 32]))), (Some(true), Some(false)));
        let mut old = hb(); old.tip_hash = None; old.wallet_state_root = None;
        assert_eq!(compare(&old, Some(([0xAB; 32], [0xCD; 32]))), (None, None), "old publisher → unknown");
    }

    #[test]
    fn view_exposes_hex_and_verdicts() {
        let v = to_view(&hb(), 42, Some(true), Some(false));
        assert_eq!(v.node, "sigil-g0-epsilon");
        assert_eq!(v.height, 4_683_730);
        assert_eq!(v.tip_hash.as_deref(), Some("abababababababababababababababababababababababababababababababab"));
        assert_eq!(v.state_root.as_deref().map(|s| s.len()), Some(64));
        assert_eq!(v.finalized, Some(4_683_218));
        assert_eq!(v.seen_ms, 42);
        assert_eq!((v.tip_matches_local, v.state_root_matches_local), (Some(true), Some(false)));
    }
}
