//! Wires `sigil-dandelion`'s protocol state machine into sigil-node's real P2P
//! transport. This is exactly the integration point the crate's own module
//! docs describe as missing — TOPIC_TXS now has a live path (see the
//! GossipsubMessage/InboundRequest branches in main.rs that route into this
//! module's `Sender`), so Dandelion++ finally has something to protect.
//!
//! # What actually gets relayed (revised 2026-08-24)
//!
//! The FIRST version of this module only covered `sigil-node::ingest`'s raw
//! `POST /tx` -> `MempoolBackend` bridge. That turned out to be the WRONG
//! live path: `/v1/send`'s handler unconditionally refuses every request
//! (`sigil_tx::SHIELDED_ONLY_HEIGHT == 0` — transparent sends are retired),
//! and `MempoolBackend`'s own pull into a real block is gated behind
//! `SIGIL_TXGEN>0` (a load-gen test flag, unset in production) — so the
//! original wiring, while real and tested, protected a path nothing actually
//! uses for money movement. The REAL live path is the shielded family
//! (`/v1/shield`, `/v1/shielded_send`, `/v1/unshield`, `/v1/shielded/
//! register`), backed by `sigil_api::shielded::ShieldedBridge`, whose
//! `snapshot_for_mint()` IS unconditionally embedded in every block
//! candidate (see `main.rs`'s `block_txs` assembly) — this is what actually
//! needs Dandelion protection. [`RelayedTx`] now covers both: `Legacy` keeps
//! the original ingest.rs path working unchanged, `Shielded` is the new,
//! actually-live-money path.
//!
//! Every shielded op is independently re-verifiable by ANY node holding its
//! signature/proof (that's what makes it safe to relay at all) — so a
//! receiving node never "trusts" that a peer already checked it. It runs the
//! SAME `submit_*` verification the origin's HTTP handler runs, just locally
//! instead of over HTTP. A resubmission of the exact same op (origin's own
//! post-submit gossip echo, or a duplicate relay) is rejected harmlessly by
//! `ShieldedBridge`'s own nullifier/nonce replay guards — not a double-apply.
//!
//! Single-actor design: ONE tokio task owns the `DandelionRouter` exclusively.
//! Routers aren't internally synchronized, and every decision already funnels
//! through one channel, so a mutex would add nothing. Other code (ingest.rs's
//! HTTP bridge, sigil-api's shielded handlers, main.rs's gossip/request
//! handlers) only ever sends a [`Cmd`] in; this actor makes the stem/fluff
//! call and drives the network + local application itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_p2p::{NetworkManager, PeerId};
use serde::{Deserialize, Serialize};
use sigil_api::shielded::{ShieldedBridge, ShieldedOp};
use sigil_dandelion::{Action, DandelionConfig, DandelionRouter};
use sigil_narwhal_mempool::MempoolBackend;
use sigil_tx::SignedTx;

/// What actually rides the wire (stem hops + TOPIC_TXS fluff). Self-
/// describing so one receive path handles both kinds without a separate
/// out-of-band tag.
#[derive(Serialize, Deserialize)]
pub enum RelayedTx {
    /// The original ingest.rs path: raw `SignedTx` JSON bytes, unchanged.
    Legacy(Vec<u8>),
    Shielded(ShieldedOp),
}

/// Bincode-serialize `relayed` and derive its Dandelion identity in one step,
/// so every call site gets a byte-identical id for byte-identical content —
/// this id is ONLY for Dandelion's own relay dedup (stopping the same wire
/// message from being reprocessed), not a replacement for `ShieldedBridge`'s
/// nullifier/nonce replay protection, which stays the real authorization
/// layer regardless of what id Dandelion uses internally.
pub fn wrap(relayed: RelayedTx) -> Option<([u8; 32], Vec<u8>)> {
    let bytes = bincode::serialize(&relayed).ok()?;
    let id = id_of(&bytes);
    Some((id, bytes))
}

/// Content-address already-wrapped wire bytes (e.g. a TOPIC_TXS gossip
/// payload) the same way [`wrap`] would have — so a receiver derives the
/// identical id an origin/relay hop already computed from the same bytes.
pub fn id_of(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// Point-to-point stem-hop wire message (flux-p2p request/response — NEVER
/// gossipsub; broadcasting this would defeat the reason Dandelion++ exists).
/// Bincode, deliberately NOT JSON: main.rs's InboundRequest handler tries
/// `BackfillReq` (JSON) first and falls through to this on failure, and a
/// binary encoding makes that fallthrough unambiguous rather than hoping no
/// field set ever collides.
#[derive(Serialize, Deserialize)]
pub struct StemWireMsg {
    pub id: [u8; 32],
    pub hops: u8,
    pub tx_bytes: Vec<u8>,
}

pub enum Cmd {
    /// This node originated it (already applied locally by the caller, if
    /// applicable — e.g. a shielded handler already ran `submit_*` before
    /// sending this). This is purely the network-propagation step.
    Originate { id: [u8; 32], bytes: Vec<u8> },
    /// A stem hop arriving point-to-point from a peer (an InboundRequest).
    StemIncoming { id: [u8; 32], hops: u8, bytes: Vec<u8> },
    /// A transaction arriving already fluffed, via TOPIC_TXS gossipsub.
    FluffIncoming { id: [u8; 32], bytes: Vec<u8> },
}

pub type Sender = tokio::sync::mpsc::UnboundedSender<Cmd>;

/// Spawn the Dandelion actor; returns the channel other code sends [`Cmd`]s into.
///
/// Also wires `shielded`'s relay hook to this actor: every op accepted by
/// `ShieldedBridge::submit_*` (already queued locally by then — see its own
/// doc comment) is handed straight to `Cmd::Originate` for network
/// propagation. `ingest.rs`'s legacy `/tx` path wires itself separately,
/// since it isn't reachable from `ShieldedBridge`.
pub fn spawn(mgr: Arc<NetworkManager>, mempool: Arc<MempoolBackend>, shielded: Arc<ShieldedBridge>) -> Sender {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    let relay_tx = tx.clone();
    shielded.set_relay_hook(move |_id, op| {
        if let Some((id, bytes)) = wrap(RelayedTx::Shielded(op)) {
            let _ = relay_tx.send(Cmd::Originate { id, bytes });
        }
    });
    tokio::spawn(async move {
        let mut router: DandelionRouter<PeerId> =
            DandelionRouter::new(DandelionConfig::default(), Instant::now());
        let mut rng = rand::rngs::OsRng;
        // Mirrors the router's own `pending` set. The router deliberately does
        // NOT retain payload bytes (see its module docs: the caller's mempool
        // already holds them) — but a still-stemming tx isn't applied locally
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
                            act(&mgr, &mempool, &shielded, &mut pending_bytes, id, action).await;
                        }
                        Cmd::StemIncoming { id, hops, bytes } => {
                            let action = router.relay_stem_incoming(id, hops, bytes, &peers, now, &mut rng);
                            act(&mgr, &mempool, &shielded, &mut pending_bytes, id, action).await;
                        }
                        Cmd::FluffIncoming { id, bytes } => {
                            // Already public — it arrived over gossip, and
                            // gossipsub's own mesh already re-broadcasts it to
                            // our peers. Only dedup + local apply; re-
                            // publishing here would just be redundant
                            // bandwidth, not a privacy or liveness gain.
                            if let Action::Fluff { bytes } = router.relay_fluff_incoming(id, bytes, now) {
                                pending_bytes.remove(&id);
                                apply_locally(&mempool, &shielded, &bytes).await;
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
                            fluff(&mgr, &mempool, &shielded, bytes).await;
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
    shielded: &Arc<ShieldedBridge>,
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
            fluff(mgr, mempool, shielded, bytes).await;
        }
        Action::Drop => {}
    }
}

/// This node deciding, for the first time, to expose a transaction to the
/// network — origination fluffed immediately, a stem hop choosing to stop
/// stemming, or the failsafe timeout. Applies locally AND publishes: nobody
/// else has broadcast this yet, so this node is the one making it public.
/// (For `Cmd::Originate` where the caller already applied it — the shielded
/// handlers do — this is a harmless, rejected-as-duplicate no-op; see the
/// module docs' note on replay guards.)
async fn fluff(mgr: &Arc<NetworkManager>, mempool: &Arc<MempoolBackend>, shielded: &Arc<ShieldedBridge>, bytes: Vec<u8>) {
    apply_locally(mempool, shielded, &bytes).await;
    if let Err(e) = mgr.publish(sigil_net::TOPIC_TXS, bytes) {
        eprintln!("⚠ dandelion: fluff publish on {} failed: {e}", sigil_net::TOPIC_TXS);
    }
}

/// Decode `bytes` as a [`RelayedTx`] and apply it through the SAME path its
/// origin's HTTP handler would have used — full independent re-verification,
/// never a "trust the peer already checked it" shortcut.
async fn apply_locally(mempool: &Arc<MempoolBackend>, shielded: &Arc<ShieldedBridge>, bytes: &[u8]) {
    let Ok(relayed) = bincode::deserialize::<RelayedTx>(bytes) else { return };
    match relayed {
        RelayedTx::Legacy(tx_json) => {
            // `mempool.ingest` is verify-once and dedup-on-repeat, so a
            // second call for a tx already held (this node's own fluff, or a
            // gossip echo of it) is a harmless no-op, not a double-apply.
            if let Ok(tx) = serde_json::from_slice::<SignedTx>(&tx_json) {
                let _ = mempool.ingest(vec![tx]);
            }
        }
        RelayedTx::Shielded(op) => {
            // Errors here are expected and harmless in the common case: the
            // origin already applied this locally before ever handing it to
            // Dandelion, so ShieldedBridge's own nullifier/nonce replay
            // guard rejects the echo — not a bug, just this node hearing
            // about its own submission a second time.
            let _ = apply_shielded_op(shielded, op);
        }
    }
}

fn apply_shielded_op(shielded: &ShieldedBridge, op: ShieldedOp) -> Result<[u8; 32], ()> {
    match op {
        ShieldedOp::Register(r) => shielded
            .submit_register(&r.wallet, &r.pk_shield, &r.pk_encrypt, r.fee, &r.sig, r.req_nonce)
            .map_err(|_| ()),
        ShieldedOp::Shield(r) => shielded
            .submit_shield(&r.from, r.amount, &r.cm, r.fee, &r.sig, r.req_nonce)
            .map_err(|_| ()),
        ShieldedOp::ShieldedSend(r) => {
            let proof = hex::decode(r.proof.trim_start_matches("0x")).map_err(|_| ())?;
            shielded
                .submit_shielded_send(&r.anchor, &r.nullifier, &r.cm_outs, r.fee, proof, &r.note_ciphertexts)
                .map_err(|_| ())
        }
        ShieldedOp::Unshield(r) => {
            let proof = hex::decode(r.proof.trim_start_matches("0x")).map_err(|_| ())?;
            shielded
                .submit_unshield(&r.to, r.amount, &r.anchor, &r.nullifier, &r.cm_outs, proof, r.fee)
                .map_err(|_| ())
        }
    }
}
