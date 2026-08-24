//! Wires `sigil-dandelion`'s protocol state machine into sigil-node's real P2P
//! transport. This is exactly the integration point the crate's own module
//! docs describe as missing — TOPIC_TXS now has a live path (see the
//! GossipsubMessage/InboundRequest branches in main.rs that route into this
//! module's `Sender`), so Dandelion++ finally has something to protect.
//!
//! Single-actor design: ONE tokio task owns the `DandelionRouter` exclusively.
//! Routers aren't internally synchronized, and every decision already funnels
//! through one channel, so a mutex would add nothing. Other code (ingest.rs's
//! HTTP bridge, main.rs's gossip/request handlers) only ever sends a [`Cmd`]
//! in; this actor makes the stem/fluff call and drives the network + mempool.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_p2p::{NetworkManager, PeerId};
use sigil_dandelion::{Action, DandelionConfig, DandelionRouter};
use sigil_narwhal_mempool::MempoolBackend;
use sigil_tx::SignedTx;

/// Point-to-point stem-hop wire message (flux-p2p request/response — NEVER
/// gossipsub; broadcasting this would defeat the reason Dandelion++ exists).
/// Bincode, deliberately NOT JSON: main.rs's InboundRequest handler tries
/// `BackfillReq` (JSON) first and falls through to this on failure, and a
/// binary encoding makes that fallthrough unambiguous rather than hoping no
/// field set ever collides.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct StemWireMsg {
    pub id: [u8; 32],
    pub hops: u8,
    pub tx_bytes: Vec<u8>,
}

pub enum Cmd {
    /// A transaction this node itself just accepted into its own mempool.
    Originate { id: [u8; 32], bytes: Vec<u8> },
    /// A stem hop arriving point-to-point from a peer (an InboundRequest).
    StemIncoming { id: [u8; 32], hops: u8, bytes: Vec<u8> },
    /// A transaction arriving already fluffed, via TOPIC_TXS gossipsub.
    FluffIncoming { id: [u8; 32], bytes: Vec<u8> },
}

pub type Sender = tokio::sync::mpsc::UnboundedSender<Cmd>;

/// Spawn the Dandelion actor; returns the channel other code sends [`Cmd`]s into.
pub fn spawn(mgr: Arc<NetworkManager>, mempool: Arc<MempoolBackend>) -> Sender {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    tokio::spawn(async move {
        let mut router: DandelionRouter<PeerId> =
            DandelionRouter::new(DandelionConfig::default(), Instant::now());
        let mut rng = rand::rngs::OsRng;
        // Mirrors the router's own `pending` set. The router deliberately does
        // NOT retain payload bytes (see its module docs: the caller's mempool
        // already holds them) — but a still-stemming tx isn't in our mempool
        // yet (see `fluff()` below), so WE hold the bytes for the failsafe
        // re-fluff on timeout.
        let mut pending_bytes: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break }; // all senders dropped — node shutting down
                    let now = Instant::now();
                    let peers = mgr.connected_peers();
                    match cmd {
                        Cmd::Originate { id, bytes } => {
                            let action = router.originate(id, bytes, &peers, now, &mut rng);
                            act(&mgr, &mempool, &mut pending_bytes, id, action).await;
                        }
                        Cmd::StemIncoming { id, hops, bytes } => {
                            let action = router.relay_stem_incoming(id, hops, bytes, &peers, now, &mut rng);
                            act(&mgr, &mempool, &mut pending_bytes, id, action).await;
                        }
                        Cmd::FluffIncoming { id, bytes } => {
                            // Already public — it arrived over gossip, and
                            // gossipsub's own mesh already re-broadcasts it to
                            // our peers. Only dedup + local mempool insert;
                            // re-publishing here would just be redundant
                            // bandwidth, not a privacy or liveness gain.
                            if let Action::Fluff { bytes } = router.relay_fluff_incoming(id, bytes, now) {
                                pending_bytes.remove(&id);
                                mempool_insert(&mempool, bytes).await;
                            }
                        }
                    };
                }
                _ = ticker.tick() => {
                    // Liveness floor: anything still stemming past the failsafe
                    // timeout gets force-fluffed now, using the bytes stashed
                    // when it first entered stem on THIS node.
                    for id in router.tick(Instant::now()) {
                        if let Some(bytes) = pending_bytes.remove(&id) {
                            fluff(&mgr, &mempool, bytes).await;
                        }
                    }
                    router.cleanup_seen(Instant::now());
                }
            }
        }
    });
    tx
}

async fn act(
    mgr: &Arc<NetworkManager>,
    mempool: &Arc<MempoolBackend>,
    pending_bytes: &mut HashMap<[u8; 32], Vec<u8>>,
    id: [u8; 32],
    action: Action<PeerId>,
) {
    match action {
        Action::RelayStem { to, hops, bytes } => {
            let wire = StemWireMsg { id, hops, tx_bytes: bytes.clone() };
            let Ok(payload) = bincode::serialize(&wire) else { return };
            pending_bytes.insert(id, bytes);
            let mgr2 = Arc::clone(mgr);
            // Fire-and-forget: a failed/timed-out stem send looks identical to
            // "the successor is just slow", and both are already covered by
            // the 30s failsafe (the ticker above) force-fluffing regardless of
            // WHY the stem stalled. Actively rerolling on send failure is a
            // real future improvement, not required for correctness.
            tokio::spawn(async move {
                let _ = mgr2.send_request(to, payload).await;
            });
        }
        Action::Fluff { bytes } => {
            pending_bytes.remove(&id);
            fluff(mgr, mempool, bytes).await;
        }
        Action::Drop => {}
    }
}

/// This node deciding, for the first time, to expose a transaction to the
/// network — origination fluffed immediately, a stem hop choosing to stop
/// stemming, or the failsafe timeout. Inserts locally AND publishes: nobody
/// else has broadcast this yet, so this node is the one making it public.
async fn fluff(mgr: &Arc<NetworkManager>, mempool: &Arc<MempoolBackend>, bytes: Vec<u8>) {
    mempool_insert(mempool, bytes.clone()).await;
    if let Err(e) = mgr.publish(sigil_net::TOPIC_TXS, bytes) {
        eprintln!("⚠ dandelion: fluff publish on {} failed: {e}", sigil_net::TOPIC_TXS);
    }
}

/// `mempool.ingest` is verify-once and dedup-on-repeat, so calling this for a
/// transaction already held (e.g. this node fluffed it, then also received
/// its own gossip echo) is a harmless no-op, not a double-apply.
async fn mempool_insert(mempool: &Arc<MempoolBackend>, bytes: Vec<u8>) {
    if let Ok(tx) = serde_json::from_slice::<SignedTx>(&bytes) {
        let _ = mempool.ingest(vec![tx]);
    }
}
