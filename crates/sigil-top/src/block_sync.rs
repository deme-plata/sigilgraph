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
                    eprintln!("[p2p-sync] start failed: {e}");
                    return;
                }
                eprintln!("[p2p-sync] started on sigil-g0 mesh (port 9501)");

                {
                    let mut s = state_clone.lock().unwrap();
                    s.running = true;
                }

                // Subscribe to blocks — event-driven, no polling
                let mut block_rx = net.subscribe(BLOCK_SYNC_TOPIC);
                let tick = Duration::from_secs(5);
                let mut last_announce = Instant::now();

                loop {
                    if stop_rx.try_recv().is_ok() {
                        let _ = net.stop().await;
                        break;
                    }

                    // Process block messages from subscription
                    while let Ok((_topic, data)) = block_rx.try_recv() {
                        if let Ok(msg) = serde_json::from_slice::<SyncMsg>(&data) {
                            match msg {
                                SyncMsg::Block { height, hash_hex, header_json } => {
                                    eprintln!("[p2p-sync] block rx: h={height}");
                                    if let Ok(header) = serde_json::from_str::<SigilBlockHeaderV0>(&header_json) {
                                        if store.put_block(header).unwrap_or(false) {
                                            let best = store.best_height();
                                            let best_hash = store.best_hash_hex();
                                            let mut s = state_clone.lock().unwrap();
                                            s.blocks_synced += 1;
                                            s.sync_height = height;
                                            s.sync_hash_hex = hash_hex.clone();
                                            s.sync_total = s.blocks_synced;
                                            s.last_message_at = Some(Instant::now());
                                            let peer_best = s.peer_best_height;
                                            drop(s);
                                            // Push progress to the P2P event channel for TUI consumption
                                            net.push_sync_progress(height, &hash_hex, peer_best, best as u64);
                                            if let Some(block) = store.get_block(&hash_hex) {
                                                new_blocks_clone.lock().unwrap().push(block);
                                            }
                                        }
                                    }
                                }
                                SyncMsg::Have { best_height, .. } => {
                                    let mut s = state_clone.lock().unwrap();
                                    if best_height > s.peer_best_height {
                                        s.peer_best_height = best_height;
                                    }
                                    s.last_message_at = Some(Instant::now());
                                }
                                SyncMsg::Req { .. } => {}
                            }
                        }
                    }

                    // Peer events from drain_events (non-block messages)
                    for event in net.drain_events() {
                        match event {
                            flux_p2p::SwarmAppEvent::PeerConnected(peer_id) => {
                                let mut s = state_clone.lock().unwrap();
                                let pc = net.peer_count();
                                s.peer_count = pc;
                                s.mesh_peer_count = pc;
                                let pid = peer_id.to_string();
                                eprintln!("[p2p-sync] peer + {pid} (total: {})", s.peer_count);
                                if pid.contains("delta") || pid.contains("5.79.79.158") {
                                    s.connected_delta = true;
                                }
                                if pid.contains("epsilon") || pid.contains("89.149.241.126") {
                                    s.connected_epsilon = true;
                                }
                            }
                            flux_p2p::SwarmAppEvent::PeerDisconnected(_) => {
                                state_clone.lock().unwrap().peer_count = net.peer_count();
                            }
                            flux_p2p::SwarmAppEvent::SyncProgress { height, hash_hex, peer_best_height, total_synced, peer_count: _ } => {
                                let mut s = state_clone.lock().unwrap();
                                s.sync_height = height;
                                s.sync_hash_hex = hash_hex;
                                s.sync_total = total_synced;
                                if peer_best_height > s.peer_best_height {
                                    s.peer_best_height = peer_best_height;
                                }
                                eprintln!("[p2p-sync] progress h={height} peer_best={peer_best_height} total={total_synced}");
                            }
                            _ => {}
                        }
                    }

                    // Periodic announcements
                    if last_announce.elapsed() >= tick {
                        last_announce = Instant::now();
                        let best = store.best_height();
                        if best > 0 {
                            let have = SyncMsg::Have {
                                best_height: best,
                                best_hash_hex: store.best_hash_hex(),
                            };
                            if let Ok(data) = serde_json::to_vec(&have) {
                                let _ = net.publish(BLOCK_SYNC_TOPIC, data);
                            }
                        }
                        let peer_best = state_clone.lock().unwrap().peer_best_height;
                        if peer_best > best && peer_best - best < 500 {
                            let req = SyncMsg::Req { from: best + 1, to: peer_best.min(best + 50) };
                            if let Ok(data) = serde_json::to_vec(&req) {
                                let _ = net.publish(BLOCK_SYNC_TOPIC, data);
                            }
                        }
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
