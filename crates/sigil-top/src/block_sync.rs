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
use std::collections::HashMap;

pub const BLOCK_SYNC_TOPIC: &str = "/sigil/g0/blocks";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum SyncMsg {
    Req { from: u64, to: u64 },
    Block { height: u64, hash_hex: String, header_json: String },
    Have { best_height: u64, best_hash_hex: String },
}
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

/// Ingest a backfill response body via the fast (out-of-order) store path. Handles both
/// the headers-only bincode format (`'H'` + `bincode(Vec<Header>)`, new nodes) and the
/// legacy full-block JSON (`BackfillResp`, old nodes). Returns the number of headers
/// stored. The store's height index + contiguous `advance()` handle out-of-order arrival,
/// so chunks from different peers can land in any order — the store IS the reorder buffer.
fn ingest_backfill_bytes(bytes: &[u8], store: &mut BlockStore) -> usize {
    // v0.10.0: collect the chunk's headers, then ONE batched write (single WAL-lock hold)
    // instead of 2 locked puts per block — the per-block path was the ingest bottleneck.
    if bytes.first() == Some(&b'H') {
        match bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&bytes[1..]) {
            Ok(headers) => store.put_blocks_batch(&headers),
            Err(_) => { crate::tlog!("[p2p-sync] resp: bad header bincode ({} B)", bytes.len()); 0 }
        }
    } else if let Ok(resp) = serde_json::from_slice::<BackfillResp>(bytes) {
        let headers: Vec<SigilBlockHeaderV0> = resp.blocks.iter()
            .filter_map(|v| v.get("header").and_then(|h| serde_json::from_value(h.clone()).ok()))
            .collect();
        store.put_blocks_batch(&headers)
    } else {
        crate::tlog!("[p2p-sync] resp: unparseable ({} bytes)", bytes.len());
        0
    }
}

/// Max block height present in a backfill/probe response body (headers-bincode or
/// legacy full-block JSON), or None if empty/unparseable. Used by the pull HEIGHT
/// PROBE to seed `peer_best_height` from a peer's actual tip — the responder clamps
/// the served range to its own tip (`hi = req.to.min(top)…`), so the max height in a
/// reply to an open-ended `[frontier, u64::MAX]` request is a real lower bound on the
/// peer's head, learnable without any gossip.
fn max_header_height(bytes: &[u8]) -> Option<u64> {
    if bytes.first() == Some(&b'H') {
        bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&bytes[1..])
            .ok()
            .and_then(|hs| hs.iter().map(|h| h.height).max())
    } else if let Ok(resp) = serde_json::from_slice::<BackfillResp>(bytes) {
        resp.blocks
            .iter()
            .filter_map(|v| v.get("header").and_then(|h| h.get("height")).and_then(|x| x.as_u64()))
            .max()
    } else {
        None
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
    /// v0.11.0: share the live sync state with the embedded explorer API (serve.rs)
    /// so `/api/v1/{status,peers}` reflect the real mesh height / verified watermark /
    /// peer count instead of being proxied to the remote node.
    pub fn state_handle(&self) -> Arc<Mutex<P2PSyncState>> {
        self.state.clone()
    }

    /// v0.13.1: seed the network tip from an EXTERNAL source (the HTTP status feed)
    /// so the backfill refill fires even when gossip AND the P2P height-probe are
    /// silent (a frozen or quiet mesh, or a producer that gossips nothing). Before
    /// this, `peer_best_height` was learnable ONLY from inbound gossip / a probe
    /// reply; on a quiet mesh it stayed 0, the `peer_best > 0` refill gate never
    /// opened, and the sync sat on "connecting" forever. Only ever RAISES the tip.
    pub fn set_known_tip(&self, height: u64) {
        if height == 0 { return; }
        let mut s = self.state.lock().unwrap();
        if height > s.peer_best_height {
            s.peer_best_height = height;
        }
    }

    pub fn launch(mut store: BlockStore) -> Self {
        let state = Arc::new(Mutex::new(P2PSyncState::default()));
        let new_blocks = Arc::new(Mutex::new(Vec::new()));
        let (stop_tx, stop_rx) = mpsc::channel();

        let state_clone = state.clone();
        let new_blocks_clone = new_blocks.clone();

        thread::spawn(move || {
            // v0.10.0: MULTI-thread runtime (3 workers). The v0.9.5 pipeline spawned chunk
            // requests as independent tasks; on a current-thread runtime those only advance
            // when the main loop awaits, so the live `SyncProgress` event flood starved them
            // → every request timed out → synced stuck at 0. Worker threads run the request
            // tasks truly concurrently, fully decoupled from the loop.
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(3).enable_all().build()
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
                // Share net into the spawned request tasks. All hot methods are &self;
                // start() (the only &mut) already ran. Arc → Send + 'static for tokio::spawn.
                let net = std::sync::Arc::new(net);

                // Resume from the PERSISTED store: seed the synced count + cursor from
                // what's already on disk so a restart/update CONTINUES instead of
                // Resume from the CONTIGUOUS synced_to (blocks 0..synced_to all present),
                // NOT best_height (a stray live block inflates it). The cursor walks
                // forward from here and never re-walks from 0.
                // v0.10.0 GENESIS ANCHOR: SIGIL's height-0 genesis is minted locally and is NOT
                // served by the range-backfill endpoint once pruned from the producer's RAM, so a
                // `from=0` request returns empty and `synced_to` could never leave 0. Anchor the
                // contiguous frontier (and verification) at the lowest servable height (1 for SIGIL;
                // env-overridable). Below `base` is never required.
                let sync_base: u64 = std::env::var("SIGIL_SYNC_BASE").ok()
                    .and_then(|s| s.parse().ok()).unwrap_or(1);
                store.set_base(sync_base);
                let resume_h = store.synced_to();
                {
                    let mut s = state_clone.lock().unwrap();
                    s.running = true;
                    s.blocks_synced = resume_h;
                    s.verified = store.verified_to(); // v0.9.0: resume the verified watermark too
                }

                // Subscribe to blocks — event-driven, no polling
                let mut block_rx = net.subscribe(BLOCK_SYNC_TOPIC);

                // ── v0.9.5 PIPELINED SLIDING-WINDOW BACKFILL ──────────────────────────
                // The v0.7.x design fired one request per peer then `join_all`-BARRIERED on
                // ALL of them with a 6s timeout — so the single slowest/behind peer gated
                // every cycle to 6s (net ≈4.9k blk/s, want ≥5k steadfast). This replaces the
                // barrier with a continuously-refilled FuturesUnordered: up to MAX_INFLIGHT
                // independent chunk requests stream in parallel; a slow peer never blocks the
                // fast ones, and the store's height-index + advance() reorder out-of-order
                // arrivals (the store IS the buffer). Reviewed with DeepSeek-V4 2026-06-09.
                const CHUNK: u64 = 8192;            // v0.12.1 perf: match the producer serve cap
                                                   // (hi = lo+8192) — 4× blocks per round-trip vs 2048.
                const MAX_INFLIGHT: usize = 16;     // v0.12.1 perf: 2× parallelism (16×8192 ≈ 128k
                                                    // blocks in flight) to hide per-serve latency → 8000 blk/s target
                // Look-ahead cap must be TIGHT: a large window lets next_start race far ahead of a
                // stalled frontier, so all MAX_INFLIGHT slots get consumed by high-range chunks that
                // don't advance synced_to while the lead chunk starves (v0.10.0 frontier-stall bug).
                // v0.12.1: was 4s — far too tight. The producers serve backfill while
                // also producing ~100 blk/s (≈54% CPU), so a ~2 MB chunk over WAN
                // routinely takes >4s → EVERY request timed out → fetched_total stuck at
                // 0 → the UI sat at "connecting…" forever. 15s lets a slow-but-alive
                // serve complete; dead peers are still benched on timeout and rerouted.
                const REQ_TIMEOUT: Duration = Duration::from_secs(15);
                const PROBE_EVERY: Duration = Duration::from_millis(500); // pull-height probe cadence
                const BENCH: Duration = Duration::from_secs(8); // bench a timed-out peer this long
                const EMPTY_BENCH: Duration = Duration::from_secs(45); // bench a peer that LACKS a needed range
                let _ = resume_h; // frontier is read live from the store each cycle (anchored, not cursored)
                // Completed request results flow back from the spawned tasks here.
                let (done_tx, mut done_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(u64, String, Option<Vec<u8>>)>();
                // pull HEIGHT-PROBE replies (open-ended range → peer's clamped tip)
                let (probe_tx, mut probe_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                let mut inflight: usize = 0;                          // outstanding spawned requests
                let mut assigned: std::collections::HashSet<u64> = std::collections::HashSet::new();
                let mut peer_bench: HashMap<String, Instant> = HashMap::new(); // peer.to_string() → benched-until
                let mut rr: usize = 0;                                 // round-robin peer cursor
                let mut last_state = Instant::now() - Duration::from_secs(1);
                let mut fetched_session: u64 = 0;                      // headers stored this session
                let mut last_verify = Instant::now() - Duration::from_secs(2); // slow verify+flush timer
                let mut last_synced_seen: u64 = resume_h;             // dynamic-base detector
                let mut last_advance_t = Instant::now();
                let mut last_probe = Instant::now() - Duration::from_secs(10); // pull-height probe timer

                loop {
                    if stop_rx.try_recv().is_ok() {
                        let _ = net.stop().await;
                        break;
                    }

                    // Process gossiped live-tip blocks — BOUNDED per iteration. The live mesh
                    // (incl. our local catching-up node) floods this topic; an UNBOUNDED drain
                    // here blocks the loop for seconds (each block costs a serde_json hash),
                    // which starved the whole pipeline (v0.10.0 synced-stuck bug). Cap it so the
                    // loop stays responsive; leftover messages drain over the next iterations.
                    let mut gdrained = 0u32;
                    while gdrained < 128 {
                        let (_topic, data) = match block_rx.try_recv() { Ok(x) => x, Err(_) => break };
                        gdrained += 1;
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
                                // NOTE: no per-event log here — the live mesh emits thousands of
                                // SyncProgress/sec; eprintln-ing each one starved the sync loop
                                // (v0.9.5 synced-stuck-at-0 bug). Progress is surfaced via state.
                            }
                            _ => {}
                        }
                    }

                    // ── DRAIN completed request results from the spawned tasks ───────────
                    while let Ok((start, peer, bytes)) = done_rx.try_recv() {
                        inflight = inflight.saturating_sub(1);
                        assigned.remove(&start);
                        match bytes {
                            Some(b) => {
                                peer_bench.remove(&peer);              // answered → healthy again
                                let got = ingest_backfill_bytes(&b, &mut store);
                                if start <= store.synced_to() + CHUNK {
                                    let hd = if b.first()==Some(&b'H') { bincode::deserialize::<Vec<sigil_header::SigilBlockHeaderV0>>(&b[1..]).ok() } else { None };
                                    let (mn,mx) = hd.as_ref().map(|h| (h.iter().map(|x|x.height).min().unwrap_or(0), h.iter().map(|x|x.height).max().unwrap_or(0))).unwrap_or((0,0));
                                    crate::tlog!("[D] LEAD start={start} got={got} h=[{mn}..{mx}] bytes={} synced={} inflight={inflight}", b.len(), store.synced_to());
                                }
                                fetched_session += got as u64;
                                // An EMPTY response over a still-needed range means this peer can't
                                // serve that range (e.g. it pruned genesis / lacks early offsets).
                                // Re-queue to the FRONT (retry promptly) AND bench this peer briefly
                                // so the range rotates to a peer that DOES serve it — otherwise the
                                // lead/genesis chunk round-robins back to the same empty peer forever
                                // and the frontier never advances (v0.10.0 synced-stuck-at-0 bug).
                                let fc = (store.synced_to() / CHUNK) * CHUNK;
                                if got == 0 && start >= fc {
                                    // EMPTY over a needed range = this peer doesn't HAVE this range
                                    // (e.g. a still-catching-up local node above its own height).
                                    // Bench it LONG so the frontier routes to full peers; the
                                    // frontier-anchored refill re-requests the range automatically.
                                    peer_bench.insert(peer, Instant::now() + EMPTY_BENCH);
                                }
                            }
                            None => {
                                if start <= store.synced_to() + CHUNK {
                                    crate::tlog!("[D] TIMEOUT start={start} peer={} synced={}", &peer[..8.min(peer.len())], store.synced_to());
                                }
                                // Timed out: bench the peer briefly so the fast peers carry the load.
                                // Refill re-requests this range (now clear of `assigned`) next cycle.
                                peer_bench.insert(peer, Instant::now() + BENCH);
                            }
                        }
                    }

                    // ── REFILL: keep the next MAX_INFLIGHT chunks AT THE FRONTIER in flight ──
                    // FRONTIER-ANCHORED (not a monotonic cursor): every cycle we (re)issue the next
                    // MAX_INFLIGHT consecutive chunks starting at the CURRENT contiguous frontier,
                    // skipping any already in flight. So the frontier chunk is ALWAYS being fetched;
                    // a stuck/slow chunk's `assigned` entry clears on completion/timeout and it's
                    // re-requested to a ROTATING peer next cycle. No cursor / retry queue / look-ahead
                    // racing past the frontier — that machinery starved the lead chunk (the chunk that
                    // actually advances synced_to) while slots went to far-ahead ranges. Out-of-order
                    // arrivals are reordered by the store (height index + advance); re-requests are
                    // idempotent. advance() here keeps the frontier fresh the instant a chunk lands.
                    store.advance();
                    let frontier_chunk = ((store.synced_to() / CHUNK) * CHUNK).max(sync_base);

                    // ── PROCESS HEIGHT-PROBE replies: seed peer_best from the peer's real tip ──
                    while let Ok(b) = probe_rx.try_recv() {
                        let got = ingest_backfill_bytes(&b, &mut store); // probe headers are free backfill
                        fetched_session += got as u64;
                        if let Some(maxh) = max_header_height(&b) {
                            let mut s = state_clone.lock().unwrap();
                            if maxh > s.peer_best_height { s.peer_best_height = maxh; }
                        }
                    }

                    // ── PULL HEIGHT PROBE (the gossip⇄backfill deadlock fix) ──────────────────
                    // peer_best was previously learnable ONLY from inbound gossip (live-tip
                    // ingest / SyncProgress). A node that connects but receives no gossip
                    // (gossipsub graft failure / silent producers) kept peer_best=0, so the
                    // refill below — gated on `peer_best > 0` and itself a PULL (send_request) —
                    // never fired a single backfill and the node stuck at base forever. This
                    // asks a healthy peer for the open-ended range [frontier, u64::MAX]; the
                    // responder CLAMPS the served range to its own tip (sigil-node main.rs:
                    // `hi = req.to.min(top)…`), so the reply's max height is a real lower bound
                    // on that peer's head — no responder change, no gossip dependency. Re-probed
                    // every PROBE_EVERY so peer_best tracks the tip and the refill self-sustains
                    // all the way to the true head (each reply also lands up to 8192 free headers).
                    if last_probe.elapsed() >= PROBE_EVERY {
                        last_probe = Instant::now();
                        let now = Instant::now();
                        let healthy: Vec<_> = net.connected_peers().into_iter()
                            .filter(|p| peer_bench.get(&p.to_string()).map_or(true, |&u| now >= u))
                            .collect();
                        if let Some(&peer) = healthy.first() {
                            let payload = serde_json::to_vec(
                                &BackfillReq { from: frontier_chunk, to: u64::MAX, headers_only: true }
                            ).unwrap();
                            let n = net.clone();
                            let tx = probe_tx.clone();
                            tokio::spawn(async move {
                                let r = tokio::time::timeout(REQ_TIMEOUT, n.send_request(peer, payload)).await;
                                if let Ok(Ok(b)) = r { let _ = tx.send(b); }
                            });
                        }
                    }

                    let peer_best = state_clone.lock().unwrap().peer_best_height;
                    if peer_best > 0 {
                        let now = Instant::now();
                        let healthy: Vec<_> = net.connected_peers().into_iter()
                            .filter(|p| peer_bench.get(&p.to_string()).map_or(true, |&u| now >= u))
                            .collect();
                        if !healthy.is_empty() {
                            for i in 0..(MAX_INFLIGHT as u64) {
                                if inflight >= MAX_INFLIGHT { break; }
                                let start = frontier_chunk + i * CHUNK;
                                if start >= peer_best { break; }          // past the tip
                                if !assigned.insert(start) { continue; }  // already in flight
                                let peer = healthy[rr % healthy.len()];
                                rr = rr.wrapping_add(1);
                                let payload = serde_json::to_vec(
                                    &BackfillReq { from: start, to: start + CHUNK, headers_only: true }
                                ).unwrap();
                                let n = net.clone();
                                let tx = done_tx.clone();
                                let peer_str = peer.to_string();
                                inflight += 1;
                                tokio::spawn(async move {
                                    let r = tokio::time::timeout(REQ_TIMEOUT, n.send_request(peer, payload)).await;
                                    let bytes = match r { Ok(Ok(b)) => Some(b), _ => None };
                                    let _ = tx.send((start, peer_str, bytes));
                                });
                            }
                        }
                    }

                    // ── FAST PERIODIC (150ms): advance the contiguous frontier + publish state ──
                    // Cheap — just walks any newly-contiguous heights and updates the TUI/window.
                    // Verification is split out to a SLOW timer below so its db-reads never gate
                    // the ingest/refill hot path (that was a v0.10.0 t_verify=3.5s stall).
                    if last_state.elapsed() >= Duration::from_millis(150) {
                        last_state = Instant::now();
                        store.advance();
                        let now_synced = store.synced_to();
                        // DYNAMIC BASE: the lowest servable height creeps UP as producers prune early
                        // history from their RAM window (the disk range-serve of pruned-low ranges is
                        // unreliable). If the frontier chunk stays unservable by ALL peers for ≥5s
                        // while we're below the tip, skip it: advance `base` one chunk and re-anchor.
                        // Self-heals to whatever the mesh actually serves (verified spine then anchors
                        // at the lowest servable height, honestly — not necessarily genesis).
                        if now_synced > last_synced_seen {
                            last_synced_seen = now_synced;
                            last_advance_t = Instant::now();
                        } else if peer_best > now_synced && last_advance_t.elapsed() >= Duration::from_secs(5) {
                            let new_base = store.base().saturating_add(CHUNK);
                            crate::tlog!("[sync] frontier {} unservable ≥5s (early history pruned from mesh) — base → {}", now_synced, new_base);
                            store.set_base(new_base);
                            last_synced_seen = store.synced_to();
                            last_advance_t = Instant::now();
                        }
                        let now_synced = store.synced_to();
                        let mut s = state_clone.lock().unwrap();
                        s.blocks_synced = now_synced;
                        s.sync_total = now_synced;
                        s.sync_cursor = now_synced;
                        s.sync_height = now_synced;
                        s.fetched_total = fetched_session;
                        s.peer_count = net.peer_count();
                        if now_synced > s.peer_best_height { s.peer_best_height = now_synced; }
                        s.last_message_at = Some(Instant::now());
                    }

                    // ── SLOW PERIODIC (1.5s): verify the spine + flush the memtable ──────────
                    // verify_to walks only NEW contiguous headers (precheck + parent linkage) and
                    // persists the watermark; a small budget keeps each pass bounded. flush() rolls
                    // the growing memtable to an SST so it can't balloon during a multi-M sync.
                    if last_verify.elapsed() >= Duration::from_millis(1500) {
                        last_verify = Instant::now();
                        // v0.15.0 perf: 40k/1.5s capped VERIFIED throughput at ~26.6k blk/s —
                        // below the 33k target and far below the verify core's measured 52k/s
                        // (chronos turbosync). 60k/1.5s lifts the verified-watermark ceiling to
                        // ~40k blk/s so the apply pipeline, not this budget, sets the rate.
                        const VERIFY_BUDGET: u64 = 60_000;
                        let report = crate::chain_verify::verify_to(&mut store, VERIFY_BUDGET);
                        let vbreak = match &report.first_break {
                            Some((h, crate::chain_verify::BreakReason::Missing)) if *h >= store.synced_to() => None,
                            Some((_h, crate::chain_verify::BreakReason::Missing)) => None,
                            Some((h, reason)) => Some(format!("h={h}: {reason}")),
                            None => None,
                        };
                        let _ = store.flush();
                        let mut s = state_clone.lock().unwrap();
                        s.verified = report.verified_to;
                        s.verify_break = vbreak;
                    }

                    // Yield: the request tasks run on worker threads (their results queue in
                    // done_rx regardless), so a short tick keeps the loop from busy-spinning
                    // while staying responsive to gossip + completions.
                    tokio::time::sleep(Duration::from_millis(10)).await;
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
