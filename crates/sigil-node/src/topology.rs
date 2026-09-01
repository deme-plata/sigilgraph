//! QTFT topology verification + peer-producer affinity. Extracted from main.rs
//! (god-file split). Pure receiver-side logic: recompute the topology commitment
//! a peer's block should carry and compare, plus a soft peer-preference hint for
//! backfill. No state writes; the braid + dag helpers come in by reference.

use crate::dag::{topology_commit_hash, window_is_complete, TOPOLOGY_COMMITMENT_WINDOW};
use sigil_dagknight::Braid;

pub(crate) const TOPOLOGY_VERIFY_HISTORY_MARGIN: u64 = 8;

/// QTFT-2 receipt-side outcome. `InsufficientHistory`, `NoWindowYet`, and
/// `WindowIncomplete` are all "nothing to compare" — distinguished only for
/// logging/telemetry, and NEVER treated as a mismatch (a node with too
/// little, or too gappy, local history has no basis to accuse an honest
/// peer). `WindowIncomplete` was added 2026-08-19/20 after a real incident:
/// a node that catches up via bulk backfill (not one-at-a-time live gossip)
/// reached `InsufficientHistory`'s live-witness threshold while its DAG
/// window for the checked heights was still gappy (eviction racing the
/// catch-up), and every single check came back Mismatch — 100% failure rate
/// from the first eligible block, on a chain independently confirmed healthy
/// (no state-root divergence, clean apply). `blocks_witnessed_live` alone
/// was not a sufficient proxy for "my window is trustworthy"; this adds the
/// direct check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TopoVerdict {
    Match,
    Mismatch,
    InsufficientHistory,
    NoWindowYet,
    WindowIncomplete,
}

/// Running counters for `verify_topology_on_receipt`, surfaced in logs (and
/// available to wire into `/v1/status` later if useful). `logged_insufficient_once`
/// keeps the boot-time "still filling in history" notice to a single line
/// instead of one per block during the fill-in period; `logged_incomplete_once`
/// does the same for the window-gap case.
#[derive(Debug, Default)]
pub(crate) struct TopologyStats {
    pub(crate) matched: u64,
    pub(crate) mismatched: u64,
    pub(crate) insufficient_history: u64,
    pub(crate) no_window_yet: u64,
    pub(crate) window_incomplete: u64,
    pub(crate) logged_insufficient_once: bool,
    pub(crate) logged_incomplete_once: bool,
}

impl TopologyStats {
    pub(crate) fn record(&mut self, v: TopoVerdict) {
        match v {
            TopoVerdict::Match => self.matched += 1,
            TopoVerdict::Mismatch => self.mismatched += 1,
            TopoVerdict::InsufficientHistory => self.insufficient_history += 1,
            TopoVerdict::NoWindowYet => self.no_window_yet += 1,
            TopoVerdict::WindowIncomplete => self.window_incomplete += 1,
        }
    }
}

/// Bound on `PeerProducerAffinity::seen` — a soft preference hint, not
/// correctness-critical state, so eviction on overflow doesn't need to be
/// precise (see `record`).
const PEER_AFFINITY_CAP: usize = 4096;

/// QTFT Path C, SIGIL-level v1 (see `SIGIL_QTFT_TOPOLOGY_v0.md`'s "knot-
/// routing in p2p" idea). The doc's original framing was a generic scoring
/// hook inside `flux-p2p` itself; that would mean teaching a chain-agnostic
/// transport crate about producers/braids, which conflicts with its own
/// "zero chain deps" boundary (mirroring `flux-topology`'s). This delivers
/// the real behavioral win at the layer that actually has both the p2p peer
/// list AND the braid/producer context: `sigil-node`.
///
/// The idea: strands that have recently crossed (merged) in the braid are
/// "topologically adjacent" — a peer relaying one is plausibly tracking the
/// other too. Concretely: remember which peer most recently delivered a
/// LIVE block from which producer, and when backfilling a gap for a known
/// producer, prefer a peer with a recent sighting of that exact producer
/// over an arbitrary connected peer.
#[derive(Default)]
pub(crate) struct PeerProducerAffinity {
    /// (peer, producer) → height of the most recent live block we saw that
    /// peer relay from that producer.
    seen: std::collections::HashMap<(flux_p2p::PeerId, [u8; 32]), u64>,
}

impl PeerProducerAffinity {
    pub(crate) fn record(&mut self, peer: flux_p2p::PeerId, producer: [u8; 32], height: u64) {
        let key = (peer, producer);
        if self.seen.len() >= PEER_AFFINITY_CAP && !self.seen.contains_key(&key) {
            // Soft bound: drop an arbitrary entry rather than track proper
            // LRU order — this only ever biases a peer preference, it never
            // gates correctness, so an imprecise evict is fine.
            if let Some(k) = self.seen.keys().next().copied() {
                self.seen.remove(&k);
            }
        }
        self.seen
            .entry(key)
            .and_modify(|h| *h = (*h).max(height))
            .or_insert(height);
    }

    /// Prefer a connected peer with a recent sighting of `producer`;
    /// otherwise fall back to the first connected peer (today's behavior).
    pub(crate) fn best_for(&self, connected: &[flux_p2p::PeerId], producer: Option<[u8; 32]>) -> Option<flux_p2p::PeerId> {
        if let Some(p) = producer {
            let mut best: Option<(flux_p2p::PeerId, u64)> = None;
            for peer in connected {
                if let Some(&h) = self.seen.get(&(*peer, p)) {
                    if best.map(|(_, bh)| h > bh).unwrap_or(true) {
                        best = Some((*peer, h));
                    }
                }
            }
            if let Some((peer, _)) = best {
                return Some(peer);
            }
        }
        connected.first().copied()
    }
}

#[cfg(test)]
mod peer_affinity_tests {
    use super::PeerProducerAffinity;
    use flux_p2p::PeerId;

    #[test]
    fn best_for_prefers_the_most_recent_producer_sighting_else_first() {
        let (a, b, c) = (PeerId::random(), PeerId::random(), PeerId::random());
        let prod = [7u8; 32];
        let mut aff = PeerProducerAffinity::default();

        // No sightings → fall back to the FIRST connected peer.
        assert_eq!(aff.best_for(&[a, b], Some(prod)), Some(a));
        // producer = None → no affinity to use → first connected peer.
        assert_eq!(aff.best_for(&[b, a], None), Some(b));
        // Nothing connected → None.
        assert_eq!(aff.best_for(&[], Some(prod)), None);

        // b has seen `prod` (h=10), a hasn't → prefer b even though a is first.
        aff.record(b, prod, 10);
        assert_eq!(aff.best_for(&[a, b], Some(prod)), Some(b));

        // c saw it more recently (h=20) → the HIGHEST-height sighting wins.
        aff.record(c, prod, 20);
        assert_eq!(aff.best_for(&[a, b, c], Some(prod)), Some(c));

        // A sighting of a DIFFERENT producer doesn't influence this one's choice.
        aff.record(a, [9u8; 32], 999);
        assert_eq!(aff.best_for(&[a, b, c], Some(prod)), Some(c));

        // record keeps the MAX height: b jumps to 25 and now beats c's 20 ...
        aff.record(b, prod, 25);
        assert_eq!(aff.best_for(&[b, c], Some(prod)), Some(b));
        // ... and a later LOWER height must not downgrade it (max-wins).
        aff.record(b, prod, 1);
        assert_eq!(aff.best_for(&[b, c], Some(prod)), Some(b));
    }
}

/// QTFT-2: recompute the topology commitment a peer's block SHOULD carry —
/// using the exact same windowed Alexander-polynomial algorithm the producer
/// used at mint time (`compute_topology_commitment`) — and compare it to
/// what the block actually claims.
///
/// Called BEFORE the block is admitted to the braid (see call site), over
/// the window `[height-32, height-1]`: strictly this block's ANCESTORS, so
/// it is well-defined whether or not `block` itself has been inserted yet.
///
/// Deliberately refuses to render a verdict until this node has personally
/// witnessed (via the live gossipsub path — never via bulk backfill/snapshot
/// restore, which could hand it a partial or differently-sourced window)
/// at least `TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN`
/// blocks since boot. Below that threshold this node's own window may be
/// incomplete relative to what the producer saw, which would manufacture
/// false mismatches against perfectly honest peers — so it reports
/// `InsufficientHistory` instead of guessing.
pub(crate) fn verify_topology_on_receipt(
    braid: &Braid,
    height: u64,
    claimed: Option<[u8; 32]>,
    blocks_witnessed_live: u64,
) -> TopoVerdict {
    if height == 0 {
        return TopoVerdict::NoWindowYet;
    }
    if blocks_witnessed_live < TOPOLOGY_COMMITMENT_WINDOW + TOPOLOGY_VERIFY_HISTORY_MARGIN {
        return TopoVerdict::InsufficientHistory;
    }
    // Check window completeness directly rather than trusting
    // `blocks_witnessed_live` as a proxy for it — a node that caught up via
    // bulk backfill can cross the live-witness threshold while its window
    // for THESE heights still has eviction gaps (see TopoVerdict's doc).
    let to_height = height - 1;
    let from_height = to_height.saturating_sub(TOPOLOGY_COMMITMENT_WINDOW.saturating_sub(1));
    let bp = braid.braid_word(from_height, to_height);
    if !window_is_complete(&bp, from_height, to_height) {
        return TopoVerdict::WindowIncomplete;
    }
    let bw = flux_topology::BraidWord { strands: bp.strands, gens: bp.word.clone() };
    let delta = flux_topology::alexander_poly(&bw);
    let expected = topology_commit_hash(&delta, bp.strands, &bp.word, &bp.producers);
    if expected == claimed {
        TopoVerdict::Match
    } else {
        TopoVerdict::Mismatch
    }
}
