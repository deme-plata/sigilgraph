//! backfill — chronos-sim proof of the **BlockRange request/response protocol**
//! (the SIGIL lightweight-node keystone, SIGIL_LIGHTWEIGHT_NODE_PLAN step 1).
//!
//! The cross-WAN propagation work proved the wall: gossip has **no history**, so a
//! late-joining light node sees live blocks at `H=k` it can't chain to genesis and
//! rejects them all. [`turbosync::run_late_join_backfill`](crate::turbosync) proved
//! the *apply* fix (backfill the gap, then go live). This module proves the **wire
//! protocol** that fetches the gap — modelled deterministically here BEFORE the real
//! flux-p2p `request_response` behaviour is built, so the design is validated for $0.
//!
//! Protocol (what the real flux-p2p behaviour will carry):
//! ```text
//!   light node detects gap (incoming H > local_height+1)
//!     → BlockRangeRequest{ from_height, to_height }   ──▶  provider (any full node)
//!     ◀── BlockRangeResponse{ blocks }  (capped at max_batch; light node loops to catch up)
//!     → apply each via apply_external_block (4-root verify) → then apply the live block
//! ```

use flux_chronos::NodeId;
use sigil_state::WalletId;
use sigil_tx::{SigilTx, NATIVE};

use crate::{demo_genesis, sign_dummy, ApplyOutcome, Block, GenesisSpec, SigilSimNode};

fn wallet(i: u8) -> WalletId {
    [i; 32]
}

/// What the light node sends when it detects a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRangeRequest {
    pub from_height: u64,
    pub to_height: u64,
}

/// What a serving peer sends back.
#[derive(Debug, Clone)]
pub struct BlockRangeResponse {
    pub blocks: Vec<Block>,
}

/// A peer that can serve block ranges (in the real net: any full node / producer).
/// Holds the canonical chain; `chain[i]` is the block at height `i + 2` (genesis = 1).
pub struct ChainProvider {
    chain: Vec<Block>,
    /// number of ranges it has served (for the protocol round-trip count).
    pub served_requests: u64,
    /// Lowest height this peer will answer for. 0 = a true full archive (the old,
    /// unconditional behaviour). Anything higher models pruned early history.
    pub serve_floor: u64,
}

impl ChainProvider {
    pub fn new(chain: Vec<Block>) -> Self {
        Self { chain, served_requests: 0, serve_floor: 0 }
    }

    /// A peer that has PRUNED early history: it answers nothing below `floor`.
    ///
    /// This models the single most common real-network property that this simulation used
    /// to assume away. `ChainProvider` held the entire chain and could not fail to serve —
    /// so every sync scenario began from "the server has everything", and an honest peer
    /// that is merely INCOMPLETE was unreachable in simulation. Chronos modelled peers that
    /// LIE (equivocation, tampered blocks, replay) but never peers that simply lack a
    /// range.
    ///
    /// That gap cost a live operator an hour: a full-archive client parked at h=30,250
    /// against a peer whose floor was 40,000, while ranges above 40,000 were served in
    /// ~700 ms each. Nothing looked wrong — ranges in flight, bytes moving, a plausible
    /// rate and ETA — and the frontier never moved, across four releases and a full store
    /// wipe.
    pub fn with_serve_floor(chain: Vec<Block>, floor: u64) -> Self {
        Self { chain, served_requests: 0, serve_floor: floor }
    }

    /// Serve `[from_height ..= to_height]`, capped at `max_batch` blocks. The light
    /// node loops (advancing `from_height`) until it's caught up.
    pub fn serve(&mut self, req: &BlockRangeRequest, max_batch: u64) -> BlockRangeResponse {
        self.served_requests += 1;
        let mut out = Vec::new();
        for b in &self.chain {
            let h = b.header.height;
            // Below the floor this peer has nothing — it answers, honestly, with less than
            // was asked for (often with nothing at all). That is NOT an error and NOT a
            // lie, which is precisely why a client can wait on it forever.
            if h < self.serve_floor {
                continue;
            }
            if h >= req.from_height && h <= req.to_height {
                out.push(b.clone());
                if out.len() as u64 >= max_batch.max(1) {
                    break;
                }
            }
        }
        BlockRangeResponse { blocks: out }
    }
}

/// Report of one light-node backfill-protocol run.
#[derive(Debug, Clone)]
pub struct LightBackfillReport {
    pub history: u64,
    pub live: u64,
    /// blocks the light node applied cleanly (gap + live).
    pub applied: u64,
    pub rejected: u64,
    pub divergences: u64,
    pub final_height: u64,
    /// how many BlockRange round-trips the protocol used.
    pub round_trips: u64,
    /// control: a gossip-only light node (no request/response) — reproduces the wall.
    pub gossip_only_applied: u64,
    pub gossip_only_height: u64,
}

impl LightBackfillReport {
    pub fn summary(&self) -> String {
        format!(
            "light backfill: {h} history + {l} live\n  gossip-only (no protocol): applied={goa} height={goh}  ← the wall\n  request/response light node: applied={ap} rejected={rj} divergence={dv} height={fh} via {rt} round-trip(s)",
            h = self.history, l = self.live,
            goa = self.gossip_only_applied, goh = self.gossip_only_height,
            ap = self.applied, rj = self.rejected, dv = self.divergences, fh = self.final_height, rt = self.round_trips,
        )
    }
}

/// Build a `history + live` chain (1 tx/block so roots change each block) from a
/// SHARED genesis — the chain and the light node MUST use the same `g` instance, or
/// the genesis hashes diverge (map-ordered) and every block's parent mismatches.
fn build_chain(g: &GenesisSpec, total: u64) -> Vec<Block> {
    let mut producer = SigilSimNode::new("producer", NodeId(0), vec![NodeId(1)], true, 1, g);
    let wallets: Vec<WalletId> = (1u8..=5).map(wallet).collect();
    let mut chain = Vec::with_capacity(total as usize);
    let mut c = 0u64;
    while (chain.len() as u64) < total {
        let from = wallets[(c % 5) as usize];
        let to = wallets[((c + 1) % 5) as usize];
        c += 1;
        producer.enqueue_tx(sign_dummy(SigilTx::Send { from, to, amount: 1, token: NATIVE, fee: 0 }));
        match producer.produce_one() {
            Some(b) => chain.push(b),
            None => break,
        }
    }
    chain
}

/// SIGIL LIGHT-NODE BACKFILL PROTOCOL (chronos-sim proof).
///
/// A producer has advanced to `history+live` blocks. A fresh light node joins late and
/// sees only the LIVE blocks (gossip). On the first live block it detects a gap and
/// runs the BlockRange request/response against a [`ChainProvider`], catching up in
/// `max_batch`-sized rounds, then applies the live stream. Contrasted with a gossip-only
/// light node (no protocol) which reproduces the late-join reject wall.
pub fn run_light_backfill_protocol(history: u64, live: u64, max_batch: u64) -> LightBackfillReport {
    let g = demo_genesis();
    let chain = build_chain(&g, history + live);
    let (_history_blocks, live_blocks) = chain.split_at(history as usize);
    let mut provider = ChainProvider::new(chain.clone());

    // ── control: gossip-only light node sees only live blocks, no protocol ──
    let mut gossip_only = SigilSimNode::new("gossip-only", NodeId(1), vec![NodeId(0)], false, 1, &g);
    let mut goa = 0u64;
    for b in live_blocks {
        if let ApplyOutcome::Ok = gossip_only.apply_external_block(b) {
            goa += 1;
        }
    }

    // ── the light node WITH the request/response protocol ──
    let mut light = SigilSimNode::new("light", NodeId(1), vec![NodeId(0)], false, 1, &g);
    let (mut applied, mut rejected, mut div, mut round_trips) = (0u64, 0u64, 0u64, 0u64);

    let mut apply = |node: &mut SigilSimNode, b: &Block, applied: &mut u64, rejected: &mut u64, div: &mut u64| {
        match node.apply_external_block(b) {
            ApplyOutcome::Ok => *applied += 1,
            ApplyOutcome::Rejected => *rejected += 1,
            ApplyOutcome::Divergence => *div += 1,
        }
    };

    for b in live_blocks {
        // detect a gap: the incoming block isn't the next one we need.
        // height() == next_height (the next block the node will accept), so a block
        // chains iff height == light.height(); anything higher leaves a gap that
        // starts AT light.height() (not +1 — that's the next-needed block itself).
        if b.header.height > light.height() {
            let mut from = light.height();
            let to = b.header.height - 1;
            // loop the request in max_batch-sized rounds until caught up.
            while from <= to {
                let resp = provider.serve(&BlockRangeRequest { from_height: from, to_height: to }, max_batch);
                round_trips += 1;
                if resp.blocks.is_empty() {
                    break;
                }
                for gb in &resp.blocks {
                    apply(&mut light, gb, &mut applied, &mut rejected, &mut div);
                    from = gb.header.height + 1;
                }
            }
        }
        // now the live block chains.
        apply(&mut light, b, &mut applied, &mut rejected, &mut div);
    }

    LightBackfillReport {
        history,
        live,
        applied,
        rejected,
        divergences: div,
        final_height: light.height(),
        round_trips,
        gossip_only_applied: goa,
        gossip_only_height: gossip_only.height(),
    }
}

#[cfg(test)]
mod tests {

    /// THE FAILURE CHRONOS COULD NOT SEE (added 2026-08-27).
    ///
    /// An honest peer that has pruned early history serves everything above its floor
    /// promptly and NOTHING below it. A client whose frontier sits below that floor can
    /// therefore fetch happily forever — bytes move, ranges complete, the rate looks
    /// healthy — while the contiguous watermark never advances by a single block.
    ///
    /// Before this, `ChainProvider` held the whole chain and could not fail to serve, so
    /// this state was not merely untested but UNREACHABLE in simulation. Every sync
    /// scenario started from "the server has everything". Chronos modelled peers that lie;
    /// it did not model peers that are honest and incomplete.
    ///
    /// Live cost: a full-archive client parked at h=30,250 against a peer with floor
    /// 40,000, for an hour, across four releases and a full store wipe — while ranges
    /// above 40,000 completed in ~700 ms each.
    #[test]
    fn a_peer_with_a_serve_floor_starves_a_lower_frontier_while_looking_healthy() {
        let chain = build_chain(&demo_genesis(), 200);
        let floor = 100u64;
        let mut peer = ChainProvider::with_serve_floor(chain, floor);

        // A client whose frontier is BELOW the floor asks for the range that would advance
        // it. The peer answers — with nothing.
        let below = BlockRangeRequest { from_height: 10, to_height: 60 };
        let got_below = peer.serve(&below, 1_000);
        assert!(
            got_below.blocks.is_empty(),
            "a peer below its serve floor must return nothing — that is the honest-but-incomplete \
             case the simulation used to assume away"
        );

        // The SAME peer serves everything above the floor perfectly. This is what makes the
        // failure so hard to see from the outside: the peer is fast, willing and correct.
        let above = BlockRangeRequest { from_height: floor, to_height: floor + 50 };
        let got_above = peer.serve(&above, 1_000);
        assert!(
            !got_above.blocks.is_empty(),
            "the same peer must serve above its floor — otherwise the test proves only that \
             the peer is broken, not that it is INCOMPLETE"
        );
        assert!(
            got_above.blocks.iter().all(|b| b.header.height >= floor),
            "nothing below the floor may leak into a higher range"
        );

        // The distinguishing signature the client now detects: repeated frontier requests
        // answered zero times, while higher ranges are answered every time.
        let mut frontier_answers = 0usize;
        let mut higher_answers = 0usize;
        for _ in 0..6 {
            if !peer.serve(&below, 1_000).blocks.is_empty() { frontier_answers += 1; }
            if !peer.serve(&above, 1_000).blocks.is_empty() { higher_answers += 1; }
        }
        assert_eq!(frontier_answers, 0, "the frontier range must never be answered");
        assert_eq!(higher_answers, 6, "while higher ranges are answered every single time");
    }

    use super::*;

    #[test]
    fn light_node_backfills_via_request_response_and_reaches_tip() {
        // 165 history (the gap Delta couldn't chain) + 20 live, 32-block batches.
        let r = run_light_backfill_protocol(165, 20, 32);
        // control: gossip-only light node applies nothing (the wall) — stuck at genesis.
        assert_eq!(r.gossip_only_applied, 0, "gossip-only must reproduce the late-join wall");
        assert_eq!(r.gossip_only_height, 1);
        // protocol: light node catches up the whole 165-gap + 20 live, no rejects.
        assert_eq!(r.applied, 185, "gap(165) + live(20) all apply via request/response");
        assert_eq!(r.rejected, 0);
        assert_eq!(r.divergences, 0);
        assert_eq!(r.final_height, 186, "genesis(1) + 165 + 20");
        // it took real round-trips (165 gap / 32 batch ≈ 6 — only on the first live block).
        assert!(r.round_trips >= 5 && r.round_trips <= 7, "got {} round-trips", r.round_trips);
    }

    #[test]
    fn batch_size_changes_round_trips_not_outcome() {
        let big = run_light_backfill_protocol(100, 10, 1000); // one round-trip
        let small = run_light_backfill_protocol(100, 10, 10); // ten round-trips
        assert_eq!(big.applied, small.applied);
        assert_eq!(big.final_height, small.final_height);
        assert_eq!(big.round_trips, 1, "one batch covers the whole gap");
        assert!(small.round_trips >= 10, "small batches → more round-trips: {}", small.round_trips);
    }

    #[test]
    fn provider_serves_capped_ranges() {
        let chain = build_chain(&demo_genesis(), 50);
        let mut p = ChainProvider::new(chain);
        let resp = p.serve(&BlockRangeRequest { from_height: 2, to_height: 51 }, 16);
        assert_eq!(resp.blocks.len(), 16, "batch cap honored");
        assert_eq!(resp.blocks[0].header.height, 2, "serves from the requested floor");
        assert_eq!(p.served_requests, 1);
    }
}
