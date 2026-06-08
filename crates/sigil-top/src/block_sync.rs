// sigil-top/src/block_sync.rs — Real P2P block sync via flux-p2p mesh (v0.7.1)
//
// v0.7.1: Uses flux_p2p::NetworkManager::for_sigil() builder and the new
// event-driven subscribe() API. No more polling — the notifier wakes us
// when blocks arrive on /sigil/g0/blocks.

use serde::{Deserialize, Serialize};
use super::block_store::BlockStore;
use sigil_header::SigilBlockHeaderV0;

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

pub const BLOCK_SYNC_TOPIC: &str = "/sigil/g0/blocks";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum SyncMsg {
    Req { from: u64, to: u64 },
    Block { height: u64, hash_hex: String, header_json: String },
    Have { best_height: u64, best_hash_hex: String },
}
// v0.7.6 content-addressed backfill now uses the shared `flux_p2p::backfill`
// protocol (BackfillMsg = Manifest/Want/Blob) on the same topic — disambiguated
// from SyncMsg by its serde tag. The inline copy was extracted into flux-p2p so
// every consumer shares one verify-don't-trust backfill engine.
use flux_p2p::backfill::{Backfill, BackfillMsg};

// v0.7.7: point-to-point backfill over the flux-p2p request-response channel.
// Wire format is shared byte-for-byte with sigil-node's server: the request is
// `serde_json::to_vec(&BackfillReq { from, to })` and the response is
// `serde_json::to_vec(&BackfillResp { blocks })`, where each element of `blocks`
// is a full Block serialized as a JSON value (same `{"header":…}` shape that's
// gossiped live on BLOCK_SYNC_TOPIC). DO NOT change these shapes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillReq {
    pub from: u64,
    pub to: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillResp {
    pub blocks: Vec<serde_json::Value>,
}

/// Ingest one block (as a serde_json::Value with a `"header"` field) exactly like
/// the live-gossip receive path: extract the SigilBlockHeaderV0, store it, and on a
/// fresh insert bump the sync counters, push a progress event, and enqueue the
/// stored block for the TUI/consumer. Returns true if a new block was stored.
fn ingest_block_value(
    v: &serde_json::Value,
    store: &mut BlockStore,
    state: &Arc<Mutex<P2PSyncState>>,
    net: &flux_p2p::NetworkManager,
    new_blocks: &Arc<Mutex<Vec<StoredBlock>>>,
) -> bool {
    let header_opt: Option<SigilBlockHeaderV0> = if let Some(h) = v.get("header") {
        serde_json::from_value(h.clone()).ok()
    } else if let Some(hj) = v.get("header_json").and_then(|x| x.as_str()) {
        serde_json::from_str(hj).ok()
    } else {
        None
    };
    let header = match header_opt {
        Some(h) => h,
        None => return false,
    };
    let height = header.height;
    let hash_hex = hex::encode(header.hash());
    if store.put_block(header).unwrap_or(false) {
        let best = store.best_height();
        let peer_best = {
            let mut s = state.lock().unwrap();
            s.blocks_synced += 1;
            s.sync_total = s.blocks_synced;
            s.sync_height = height;
            s.sync_hash_hex = hash_hex.clone();
            if height > s.peer_best_height {
                s.peer_best_height = height;
            }
            s.last_message_at = Some(Instant::now());
            s.peer_best_height
        };
        net.push_sync_progress(height, &hash_hex, peer_best, best);
        if let Some(block) = store.get_block(&hash_hex) {
            new_blocks.lock().unwrap().push(block);
        }
        true
    } else {
        let mut s = state.lock().unwrap();
        if height > s.peer_best_height {
            s.peer_best_height = height;
        }
        s.last_message_at = Some(Instant::now());
        false
    }
}

#[derive(Debug, Clone, Default)]
pub struct P2PSyncState {
    pub running: bool,
    pub peer_count: u32,
    pub mesh_peer_count: u32,
    pub peer_best_height: u64,
    pub blocks_synced: u64,
    pub last_message_at: Option<Instant>,
    pub connected_delta: bool,
    pub connected_epsilon: bool,
    /// v0.7.0: Latest sync progress from the P2P mesh (consumed by TUI gauge).
    pub sync_height: u64,
    pub sync_hash_hex: String,
    pub sync_total: u64,
    /// v0.7.6: blocks pulled via content-addressed backfill (flux-sync), verified.
    pub backfilled: u64,
}

pub struct P2PBlockSync {
    state: Arc<Mutex<P2PSyncState>>,
    new_blocks: Arc<Mutex<Vec<StoredBlock>>>,
    stop_tx: Option<mpsc::Sender<()>>,
}

pub use super::block_store::StoredBlock;

impl P2PBlockSync {
    pub fn launch(mut store: BlockStore) -> Self {
        let state = Arc::new(Mutex::new(P2PSyncState::default()));
        let new_blocks = Arc::new(Mutex::new(Vec::new()));
        let (stop_tx, stop_rx) = mpsc::channel();

        let state_clone = state.clone();
        let new_blocks_clone = new_blocks.clone();

        thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all().build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };

            rt.block_on(async move {
                // v0.7.1: Use for_sigil() — preconfigured for port 9501 + SIGIL topics
                let mut net = flux_p2p::NetworkManager::for_sigil("top");

                if let Err(e) = net.start().await {
                    crate::tlog!("[p2p-sync] start failed: {e}");
                    return;
                }
                crate::tlog!("[p2p-sync] started on sigil-g0 mesh (port 9501)");

                {
                    let mut s = state_clone.lock().unwrap();
                    s.running = true;
                }

                // v0.7.6: content-addressed backfill via the shared flux_p2p::backfill
                // engine (flux-sync over flux-aether). Path overridable; default under
                // $HOME. Backfill is simply disabled if the store can't open (graceful).
                let store_dir = std::env::var("SIGIL_SYNC_STORE").unwrap_or_else(|_| {
                    std::env::var("HOME")
                        .map(|h| format!("{h}/.sigil-sync-blocks"))
                        .unwrap_or_else(|_| "sigil-sync-blocks".into())
                });
                let mut bf = Backfill::open(&store_dir, "sigil-g0").ok();
                if bf.is_none() {
                    crate::tlog!("[p2p-sync] flux_p2p::backfill store unavailable — backfill disabled");
                }

                // Subscribe to blocks — event-driven, no polling
                let mut block_rx = net.subscribe(BLOCK_SYNC_TOPIC);
                let tick = Duration::from_secs(5);
                let mut last_announce = Instant::now();
                let mut last_bf = Instant::now();
                let mut sync_cursor: u64 = 0;

                loop {
                    if stop_rx.try_recv().is_ok() {
                        let _ = net.stop().await;
                        break;
                    }

                    // Process block messages from subscription
                    while let Ok((_topic, data)) = block_rx.try_recv() {
                        // sigil-node broadcasts raw Block JSON ({"header":{…},…})
                        // on TOPIC_BLOCKS. Parse the header out and STORE it — THIS is
                        // what fills the DB genesis→tip. Also accept legacy
                        // {"header_json":"…"}; ignore our own SyncReq echoes.
                        let v: serde_json::Value = match serde_json::from_slice(&data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if v.get("sync_from").is_some() { continue; }
                        ingest_block_value(&v, &mut store, &state_clone, &net, &new_blocks_clone);
                    }

                    // Peer events from drain_events (non-block messages)
                    for event in net.drain_events() {
                        match event {
                            flux_p2p::SwarmAppEvent::PeerConnected { peer_id, addr } => {
                                let mut s = state_clone.lock().unwrap();
                                let pc = net.peer_count();
                                s.peer_count = pc;
                                s.mesh_peer_count = pc;
                                // Δ/Ε detection by IP — the peer_id is a base58 hash and
                                // never contains "delta"/"epsilon"; the remote multiaddr
                                // carries the real IP, so match on that.
                                if addr.contains("5.79.79.158") {
                                    s.connected_delta = true;
                                }
                                if addr.contains("89.149.241.126") {
                                    s.connected_epsilon = true;
                                }
                                crate::tlog!("[p2p-sync] peer + {peer_id} @ {addr} (total: {pc})");
                            }
                            flux_p2p::SwarmAppEvent::PeerDisconnected { .. } => {
                                let pc = net.peer_count();
                                let mut s = state_clone.lock().unwrap();
                                s.peer_count = pc;
                                s.mesh_peer_count = pc;
                                if pc == 0 {
                                    s.connected_delta = false;
                                    s.connected_epsilon = false;
                                }
                            }
                            flux_p2p::SwarmAppEvent::SyncProgress { height, hash_hex, peer_best_height, total_synced, peer_count: _ } => {
                                let mut s = state_clone.lock().unwrap();
                                s.sync_height = height;
                                s.sync_hash_hex = hash_hex;
                                s.sync_total = total_synced;
                                if peer_best_height > s.peer_best_height {
                                    s.peer_best_height = peer_best_height;
                                }
                                crate::tlog!("[p2p-sync] progress h={height} peer_best={peer_best_height} total={total_synced}");
                            }
                            _ => {}
                        }
                    }

                    // Fast backfill: walk genesis→tip in chunks via the flux-p2p
                    // request-response channel (point-to-point, NOT gossip). We pick a
                    // connected peer, send a BackfillReq{from,to}, and ingest every block in
                    // the BackfillResp. The cursor LOOPS continuously (0→tip→0) so dropped
                    // chunks get refilled — this is what actually reaches 100% (the old
                    // gossip cursor stalled partway, e.g. "stuck at 22%").
                    //
                    // send_request().await is inline here: this is the monitor's single sync
                    // thread, so pausing the loop during the request is fine.
                    if last_bf.elapsed() >= Duration::from_millis(300) {
                        last_bf = Instant::now();
                        let (peer_best, have) = {
                            let s = state_clone.lock().unwrap();
                            (s.peer_best_height, s.blocks_synced)
                        };
                        if peer_best > 0 && have < peer_best {
                            if let Some(peer) = net.connected_peers().into_iter().next() {
                                let from = sync_cursor;
                                let to = from + 1024;
                                let req = BackfillReq { from, to };
                                crate::tlog!("[p2p-sync] backfill req→{peer} [{from}..={to}] (have {have}/{peer_best})");
                                match net.send_request(peer, serde_json::to_vec(&req).unwrap()).await {
                                    Ok(bytes) => {
                                        if let Ok(resp) = serde_json::from_slice::<BackfillResp>(&bytes) {
                                            let mut stored = 0u64;
                                            for v in &resp.blocks {
                                                if ingest_block_value(v, &mut store, &state_clone, &net, &new_blocks_clone) {
                                                    stored += 1;
                                                }
                                            }
                                            crate::tlog!("[p2p-sync] backfill resp: {} blocks ({stored} new)", resp.blocks.len());
                                        }
                                    }
                                    Err(_) => {}
                                }
                                sync_cursor = if to >= peer_best { 0 } else { to + 1 }; // loop to refill gaps
                            }
                        }
                    }

                    // Slow announce: peer-count refresh.
                    if last_announce.elapsed() >= tick {
                        last_announce = Instant::now();
                        state_clone.lock().unwrap().peer_count = net.peer_count();
                    }

                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        });

        P2PBlockSync { state, new_blocks, stop_tx: Some(stop_tx) }
    }

    pub fn poll_state(&self) -> P2PSyncState {
        self.state.lock().unwrap().clone()
    }

    pub fn drain_new_blocks(&self) -> Vec<StoredBlock> {
        std::mem::take(&mut *self.new_blocks.lock().unwrap())
    }
}

impl Drop for P2PBlockSync {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}
