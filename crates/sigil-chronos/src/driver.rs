//! CHRONOS-T driver — runs a [`SigilSimNode`] over a real [`Transport`] in
//! wall-clock time.
//!
//! This is the wall-clock twin of `flux_chronos::Universe::advance`. The
//! Universe drives a SimNode under a virtual clock for deterministic sim;
//! this driver drives the SAME SimNode under real time over real flux-p2p.
//! One chain-logic code path, two transports.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flux_chronos::{Envelope, NodeId, SimNode};

use crate::transport::Transport;
use crate::SigilSimNode;

/// Wall-clock micros since epoch — the `now` the SimNode sees over the wire.
fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Topic blocks gossip on. Matches `sigil-net`'s `TOPIC_BLOCKS`.
pub const TOPIC_BLOCKS: &str = "/sigil/g0/blocks";

/// Run `node` over `transport` for `run_secs` wall-clock seconds.
///
/// - Producers mint one block per `block_time_ms` (gated on ≥1 peer being up
///   so blocks aren't lost into an empty mesh), publishing each to
///   [`TOPIC_BLOCKS`].
/// - Followers apply every received block through their own chokepoint and
///   log the outcome (Ok / Divergence / Rejected).
///
/// Returns the node so the caller can read final counters (blocks_applied,
/// divergence_count, …).
pub fn run<T: Transport>(
    mut node: SigilSimNode,
    mut transport: T,
    block_time_ms: u64,
    run_secs: u64,
) -> SigilSimNode {
    let start = Instant::now();
    let mut last_produce = Instant::now();
    let poll_interval = Duration::from_millis((block_time_ms / 4).max(50));

    loop {
        if start.elapsed().as_secs() >= run_secs {
            break;
        }

        // 1. Drain real gossipsub → synthesize envelopes. The `to`/`from`
        //    NodeIds are synthetic: SigilSimNode.step dispatches on the TAG
        //    byte in the payload, not on routing identity.
        let incoming: Vec<Envelope> = transport
            .poll()
            .into_iter()
            .map(|(_topic, data)| Envelope {
                from: NodeId(u32::MAX),
                to: NodeId(0),
                sent_at: now_micros(),
                payload: data,
            })
            .collect();

        // 2. Decide whether to step. Producers step on cadence (once a peer
        //    is up) OR whenever something arrived; followers step on arrival.
        let peer_up = transport.peer_count() > 0;
        let produce_due =
            node.is_producer() && peer_up && last_produce.elapsed().as_millis() as u64 >= block_time_ms;

        if !incoming.is_empty() || produce_due {
            let result = node.step(now_micros(), &incoming);

            // 3. Dedup publishes by payload (producer emits one per peer; on
            //    the wire that collapses to a single topic publish).
            let mut seen: HashSet<[u8; 32]> = HashSet::new();
            for env in result.publish {
                let id = *blake3::hash(&env.payload).as_bytes();
                if seen.insert(id) {
                    transport.publish(TOPIC_BLOCKS, env.payload);
                }
            }
            for ev in result.events {
                println!("[{}us] {ev}", now_micros());
            }
            if produce_due {
                last_produce = Instant::now();
            }
        }

        std::thread::sleep(poll_interval);
    }

    node
}
