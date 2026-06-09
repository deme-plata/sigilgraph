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
    /// v0.7.27: the monitor only stores headers — ask for headers-only so the node
    /// replies with a compact bincode `Vec<SigilBlockHeaderV0>` (≈20× less wire, no
    /// JSON to lex). Old nodes ignore it and reply with full-block JSON (we fall back).
    #[serde(default)]
    pub headers_only: bool,
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
        let synced = store.synced_to(); // contiguous progress (not raw count)
        let peer_best = {
            let mut s = state.lock().unwrap();
            s.blocks_synced = synced;        // contiguous tip (bar/⬇/chunk all use this)
            s.sync_total = s.blocks_synced;
            s.sync_cursor = synced;          // chunk shows [synced..synced+chunk] = next needed
            s.fetched_total += 1;            // smooth, monotonic — drives the rate readout
            s.sync_height = synced;          // ✓ badge tracks the contiguous tip, not a stale height
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
    /// v0.7.11: the height the in-flight request-response backfill chunk starts at
    /// (the TUI shows the [from..from+chunk] range being fetched).
    pub sync_cursor: u64,
    /// v0.7.26: monotonic count of blocks RECEIVED+stored this session (NOT the
    /// contiguous tip — that's `blocks_synced`). Drives the rate readout so it's
    /// smooth: the contiguous tip advances in bursts (gap-fills) and would read 0
    /// between jumps, while this climbs continuously while fetching.
    pub fetched_total: u64,
    /// v0.9.0: contiguous CRYPTOGRAPHICALLY-VERIFIED tip — blocks 0..verified each passed
    /// precheck + parent-linkage (spine connects back to genesis). `blocks_synced` means
    /// "downloaded"; THIS means "downloaded AND validated as one chain". The full-sync
    /// completion gate watches this, not blocks_synced.
    pub verified: u64,
    /// v0.9.0: set when the verifier hit a real integrity break (NOT the clean download
    /// frontier): "(height) reason". Empty while the chain is clean. Surfaced in the TUI
    /// + makes `full-sync`/`verify-chain` exit non-zero.
    pub verify_break: Option<String>,
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

                // Resume from the PERSISTED store: seed the synced count + cursor from
                // what's already on disk so a restart/update CONTINUES instead of
                // Resume from the CONTIGUOUS synced_to (blocks 0..synced_to all present),
                // NOT best_height (a stray live block inflates it). The cursor walks
                // forward from here and never re-walks from 0.
                let resume_h = store.synced_to();
                {
                    let mut s = state_clone.lock().unwrap();
                    s.running = true;
                    s.blocks_synced = resume_h;
                    s.verified = store.verified_to(); // v0.9.0: resume the verified watermark too
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
                let mut sync_cursor: u64 = resume_h; // continue from persisted tip, not 0
                // Per-peer COOLDOWN (keyed by cycle index): a peer that times out is
                // benched for N cycles regardless of partial successes on other ranges.
                // A diverged/behind peer serves LOW ranges but times out on HIGH ones, so
                // a plain reset-on-success counter never benches it; the cooldown does.
                let mut peer_cd: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
                let mut cycle: u64 = 0;

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
                    // PARALLEL backfill: fan out one request per connected peer, each for a
                    // DISTINCT chunk, and await them concurrently (join_all). One-at-a-time
                    // inline await was the bottleneck — each 8192-block (~24MB) response takes
                    // seconds, so serializing them capped throughput; N peers in parallel is ~Nx.
                    // Tight cadence (50ms) so cycles pipeline back-to-back.
                    if last_bf.elapsed() >= Duration::from_millis(50) {
                        last_bf = Instant::now();
                        let (peer_best, have) = {
                            let s = state_clone.lock().unwrap();
                            (s.peer_best_height, s.blocks_synced)
                        };
                        if peer_best > 0 && have < peer_best {
                            cycle += 1;
                            let all_peers = net.connected_peers();
                            // Route only to peers not on cooldown (converges to the fast,
                            // fully-synced servers). If everyone's cooling down, retry all.
                            let mut candidates: Vec<_> = all_peers.iter().cloned()
                                .filter(|p| cycle >= *peer_cd.get(&p.to_string()).unwrap_or(&0)).collect();
                            if candidates.is_empty() && !all_peers.is_empty() {
                                peer_cd.clear();
                                candidates = all_peers.clone();
                            }
                            if !candidates.is_empty() {
                                const CHUNK: u64 = 8192;
                                // Walk FORWARD from the contiguous synced_to; one consecutive
                                // chunk per healthy peer, rotating which peer gets the lead chunk.
                                let cursor = store.synced_to();
                                state_clone.lock().unwrap().sync_cursor = cursor; // TUI
                                let netref = &net;
                                let nc = candidates.len();
                                let rot = sync_cursor as usize;
                                let reqs = (0..nc).map(|i| {
                                    let from = cursor + i as u64 * CHUNK;
                                    let peer = candidates[(i + rot) % nc];
                                    let payload = serde_json::to_vec(&BackfillReq { from, to: from + CHUNK, headers_only: true }).unwrap();
                                    // 3s cap so a slow/dead peer can't gate the cycle.
                                    async move {
                                        let r = match tokio::time::timeout(Duration::from_secs(6), netref.send_request(peer, payload)).await {
                                            Ok(Ok(bytes)) => Some(bytes),
                                            _ => None,
                                        };
                                        (peer, r)
                                    }
                                });
                                crate::tlog!("[p2p-sync] backfill: {} reqs ({} healthy/{} peers) from {} (have {have}/{peer_best})", nc, candidates.len(), all_peers.len(), cursor);
                                let t_net0 = Instant::now();
                                let results = futures::future::join_all(reqs).await;
                                let t_net = t_net0.elapsed();
                                let t_ing0 = Instant::now();
                                // Lean batch ingest: store via the fast path, advance + update
                                // TUI state ONCE. Track per-peer responsiveness.
                                let mut got = 0usize;
                                for (peer, r) in results {
                                    let bytes = match r {
                                        Some(b) => { peer_cd.remove(&peer.to_string()); b }
                                        None => { peer_cd.insert(peer.to_string(), cycle + 3); continue; } // bench 3 cycles
                                    };
                                    if bytes.first() == Some(&b'H') {
                                        // headers-only bincode (new node, fast path)
                                        match bincode::deserialize::<Vec<sigil_header::SigilBlockHeaderV0>>(&bytes[1..]) {
                                            Ok(headers) => {
                                                for header in headers { let _ = store.put_block_fast(header); got += 1; }
                                            }
                                            Err(_) => crate::tlog!("[p2p-sync] resp: bad header bincode ({} B)", bytes.len()),
                                        }
                                    } else if let Ok(resp) = serde_json::from_slice::<BackfillResp>(&bytes) {
                                        // full-block JSON (old node, fallback)
                                        for v in &resp.blocks {
                                            if let Some(h) = v.get("header") {
                                                if let Ok(header) = serde_json::from_value::<sigil_header::SigilBlockHeaderV0>(h.clone()) {
                                                    let _ = store.put_block_fast(header);
                                                    got += 1;
                                                }
                                            }
                                        }
                                    } else {
                                        crate::tlog!("[p2p-sync] resp: unparseable ({} bytes)", bytes.len());
                                    }
                                }
                                store.advance();
                                let now_synced = store.synced_to();
                                // v0.9.0 FULL VERIFYING SYNC: validate the freshly-downloaded
                                // prefix as a connected spine (precheck + parent linkage) before
                                // calling it "synced". Bounded per batch so a multi-M-block chain
                                // stays responsive; the watermark persists + resumes. A real break
                                // (not the download frontier) is surfaced loudly.
                                const VERIFY_BUDGET: u64 = 50_000;
                                let report = crate::chain_verify::verify_to(&mut store, VERIFY_BUDGET);
                                let vbreak = match &report.first_break {
                                    Some((h, crate::chain_verify::BreakReason::Missing)) if *h >= now_synced => None,
                                    Some((h, reason)) => Some(format!("h={h}: {reason}")),
                                    None => None,
                                };
                                {
                                    let mut s = state_clone.lock().unwrap();
                                    s.blocks_synced = now_synced;
                                    s.sync_total = now_synced;
                                    s.sync_cursor = now_synced;
                                    s.sync_height = now_synced;
                                    s.fetched_total += got as u64;
                                    s.verified = report.verified_to;
                                    s.verify_break = vbreak;
                                    if now_synced > s.peer_best_height { s.peer_best_height = now_synced; }
                                    s.last_message_at = Some(Instant::now());
                                }
                                crate::tlog!("[p2p-sync] CYCLE: {} blk · net {:?} · ingest {:?} · synced {} → {}",
                                    got, t_net, t_ing0.elapsed(), cursor, now_synced);
                                sync_cursor = sync_cursor.wrapping_add(1); // rotation counter
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
