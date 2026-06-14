// sigil-top/src/block_sync.rs — Real P2P block sync via flux-p2p mesh (v0.7.1)
//
// v0.7.1: Uses flux_p2p::NetworkManager::for_sigil() builder and the new
// event-driven subscribe() API. No more polling — the notifier wakes us
// when blocks arrive on /sigil/g0/blocks.

use serde::{Deserialize, Serialize};
use super::block_store::BlockStore;
use sigil_header::SigilBlockHeaderV0;

use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use std::collections::HashMap;

// ── 0.77: ONE process-wide pooled HTTP client per flavor ─────────────────────────────
// The old pattern built a fresh reqwest Client per poll/thread — every build opens a new
// TCP+TLS connection that then sits in TIME_WAIT for ~60s. Over a multi-hour genesis
// archive sync that exhausted Windows' ephemeral ports (the "tip frozen / error sending
// request" bug, #156 item 3). A shared client = keep-alive reuse + a bounded idle pool;
// per-call timeouts stay exactly as they were (set on the builder here).
static HTTP_BLOCKING: std::sync::LazyLock<reqwest::blocking::Client> =
    std::sync::LazyLock::new(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    });
static HTTP_ASYNC: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

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
    /// v0.33 (1M-blk/s lane): requested response codec. 0 = raw `'H'+bincode`,
    /// 1 = `'Z'+zstd(bincode)` — MEASURED 14.0× on a real 4096-header chunk (1019 B →
    /// ~73 B/header), beating lz4 (11.0×) at the same compress speed AND faster decomp.
    /// Compat both ways: old servers ignore unknown JSON fields → reply 'H' (we still
    /// decode it); old clients omit the field → serde defaults 0 → new servers reply 'H'.
    #[serde(default)]
    pub codec: u8,
}

/// Decompress a `'Z'` zstd wire body (pure-Rust ruzstd — no C in the Windows cross-build).
/// CAPPED at 64 MB output: a malicious peer must not zstd-bomb the monitor (a real chunk
/// decompresses to ≤ ~8 MB). None on any malformed/oversized stream — caller treats it
/// exactly like an unparseable response (logged, peer benched), never a panic.
/// v0.59: how far a gossip-claimed tip may LEAD the signed oracle before it's a phantom.
/// Generous enough for sub-second gossip liveness ahead of the ~1s oracle cadence, tight
/// enough to reject a post-genesis-reset ghost (a 1.4M claim while the oracle is at 0.88M).
const SANE_LEAD: u64 = 65_536;

/// v0.39/v0.59: bounded-raise guard for PEER-claimed tips. peer_best only-raises, so a single
/// bogus gossip claim (the 26.8M / post-reset 1.4M phantom) used to poison the sync target. The
/// signed `sigil-tip-live.json` oracle is the AUTHORITY: once it has answered (`oracle > 0`), a
/// gossip claim may lead it by at most `SANE_LEAD`; wilder jumps are ignored. Before any oracle
/// answers (offline / cold CDN) fall back to a bounded raise off the current belief.
fn sane_raise(oracle: u64, cur: u64, claim: u64) -> bool {
    if oracle > 0 {
        claim <= oracle.saturating_add(SANE_LEAD)
    } else {
        cur == 0 || claim <= cur.saturating_add(2_000_000)
    }
}

#[cfg(test)]
mod oracle_anchor_tests {
    use super::{sane_raise, SANE_LEAD};
    #[test]
    fn oracle_caps_phantom_gossip() {
        // oracle 0.88M: the post-reset 1.4M phantom is rejected; a small lead is allowed.
        assert!(!sane_raise(883_000, 883_000, 1_400_000), "1.4M phantom must be rejected");
        assert!(sane_raise(883_000, 883_000, 883_000 + 1_000), "small gossip lead ok");
        assert!(sane_raise(883_000, 883_000, 883_000 + SANE_LEAD), "exactly the lead ok");
        assert!(!sane_raise(883_000, 883_000, 883_000 + SANE_LEAD + 1), "just past the lead rejected");
    }
    #[test]
    fn pre_oracle_falls_back_to_bounded_raise() {
        // oracle == 0 (cold boot, offline): bounded +2M off current; cur==0 seeds anything.
        assert!(sane_raise(0, 0, 5_000_000), "cold seed allowed");
        assert!(sane_raise(0, 1_000_000, 2_900_000), "within +2M ok");
        assert!(!sane_raise(0, 1_000_000, 3_100_001), "beyond +2M rejected");
    }
}

fn zstd_decompress_body(body: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    // v0.39: 64 MiB was 8x too generous — a real chunk is <= ~8 MB, and the worst-case
    // burst is MAX_OUT x inflight slots DURING STARTUP (before the first frame). 12 MiB
    // keeps full headroom while capping the burst ~6x lower.
    const MAX_OUT: u64 = 12 * 1024 * 1024;
    let mut dec = ruzstd::StreamingDecoder::new(body).ok()?;
    let mut out = Vec::new();
    dec.take(MAX_OUT + 1).read_to_end(&mut out).ok()?;
    if out.len() as u64 > MAX_OUT { return None; } // bomb guard
    Some(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillResp {
    pub blocks: Vec<serde_json::Value>,
}

/// v0.50 (LANE-A sync): chunk-align a base height to the nearest CHUNK boundary at/below `h`,
/// clamped to the lowest servable height `sync_base`. Used by the recent-window probe-before-snap.
fn align_base(h: u64, chunk: u64, sync_base: u64) -> u64 {
    ((h / chunk) * chunk).max(sync_base)
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
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.blocks_synced = synced;        // contiguous tip (bar/⬇/chunk all use this)
            s.sync_total = s.blocks_synced;
            s.sync_cursor = synced;          // chunk shows [synced..synced+chunk] = next needed
            s.fetched_total += 1;            // smooth, monotonic — drives the rate readout
            s.sync_height = synced;          // ✓ badge tracks the contiguous tip, not a stale height
            s.sync_hash_hex = hash_hex.clone();
            if height > s.peer_best_height && sane_raise(s.oracle_tip, s.peer_best_height, height) {
                s.peer_best_height = height;
            }
            s.last_message_at = Some(Instant::now());
            s.peer_best_height
        };
        net.push_sync_progress(height, &hash_hex, peer_best, best);
        if let Some(block) = store.get_block(&hash_hex) {
            // v0.26 (DeepSeek-hardened): cap the hand-off buffer so a slow or absent consumer
            // (headless `full-sync`, or a UI that briefly stalls) can't grow it unbounded over a
            // multi-million-block sync — a real OOM risk on a long-running operator terminal.
            // Keep the newest 10k; a live monitor only ever renders the tail anyway.
            let mut nb = new_blocks.lock().unwrap_or_else(|e| e.into_inner());
            nb.push(block);
            let n = nb.len();
            if n > 10_000 { nb.drain(0..n - 10_000); }
        }
        true
    } else {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if height > s.peer_best_height && sane_raise(s.oracle_tip, s.peer_best_height, height) {
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
    // v0.33: 'Z' = zstd-compressed headers (codec=1 reply). Decompress (capped) → same
    // bincode Vec<Header> body as 'H'. 14× less wire, measured on real chunks.
    if bytes.first() == Some(&b'Z') {
        return match zstd_decompress_body(&bytes[1..]) {
            Some(body) => match bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&body) {
                Ok(headers) => store.put_blocks_batch(&headers),
                Err(_) => { crate::tlog!("[p2p-sync] resp: bad zstd header bincode ({} B)", bytes.len()); 0 }
            },
            None => { crate::tlog!("[p2p-sync] resp: zstd decompress failed ({} B)", bytes.len()); 0 }
        };
    }
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
    if bytes.first() == Some(&b'Z') {
        // v0.33: zstd reply — decompress (capped) then scan like the 'H' body.
        let body = zstd_decompress_body(&bytes[1..])?;
        return bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&body)
            .ok()
            .and_then(|hs| hs.iter().map(|h| h.height).max());
    }
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

/// Min+max block height present in a backfill response body, across ALL wire codecs
/// (`'Z'` zstd, `'H'` raw-bincode, legacy JSON). Mirrors `max_header_height` but returns
/// the full `[lo..hi]` range — used by the `[D]` sync debug line so the operator sees the
/// REAL heights in a chunk. (Before v0.38.1 that line only decoded `'H'`, so once the
/// zstd codec=1 lane went live every chunk logged `h=[0..0]` and looked like a broken
/// decode — a pure display bug; `ingest_backfill_bytes` always stored the blocks fine.)
fn header_height_range(bytes: &[u8]) -> Option<(u64, u64)> {
    let heights: Vec<u64> = if bytes.first() == Some(&b'Z') {
        let body = zstd_decompress_body(&bytes[1..])?;
        bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&body).ok()?.iter().map(|h| h.height).collect()
    } else if bytes.first() == Some(&b'H') {
        bincode::deserialize::<Vec<SigilBlockHeaderV0>>(&bytes[1..]).ok()?.iter().map(|h| h.height).collect()
    } else if let Ok(resp) = serde_json::from_slice::<BackfillResp>(bytes) {
        resp.blocks.iter()
            .filter_map(|v| v.get("header").and_then(|h| h.get("height")).and_then(|x| x.as_u64()))
            .collect()
    } else {
        return None;
    };
    Some((*heights.iter().min()?, *heights.iter().max()?))
}

/// Fetch the network's REAL tip height from the published `sigil-tip-live.json`.
/// v0.17.0: the monitor's `/api/v1/status` mis-routes to a near-empty sigil-rpcd that
/// returns height=2, so `set_known_tip` seeded `peer_best≈2` and the fast-snap (gated on
/// `peer_best > synced+200k`) NEVER fired → genesis crawl at ~1 blk/s. The probe is
/// clamped to frontier+CHUNK so it can't reveal the tip either. This signed-by-producer
/// JSON carries the true height (~6.7M), so it's the reliable tip source for the snap.
async fn fetch_live_tip() -> Option<u64> {
    // 0.77: shared pooled client (keep-alive) — was a fresh Client per call.
    fetch_live_tip_inner(&HTTP_ASYNC).await
}

/// Blocking variant — runs on a DEDICATED OS thread, isolated from the busy block_sync
/// tokio runtime where the async fetch was non-deterministically starved (peer_best froze
/// → the monitor parked behind the tip). This is the reliable peer_best source.
fn fetch_live_tip_blocking() -> Option<u64> {
    const URLS: [&str; 2] = [
        "https://sigilgraph.fluxapp.xyz/sigil-tip-live.json",
        "https://quillon.xyz/sigil-tip-live.json",
    ];
    let cb = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    // v0.29.5 (sync hardening): RACE the two CDN oracles instead of trying them sequentially.
    // The old loop hit URL[0] first and, if that CDN was slow or down, BLOCKED the full 5 s
    // timeout before even trying URL[1] — so a single bad edge node stalled cold-start tip
    // acquisition (and every poll) for 5 s. Now both fire concurrently and the FIRST positive
    // height wins, so the monitor's fast-snap fires as soon as ANY oracle answers. A dead CDN
    // costs nothing; resilience to a one-CDN outage is free.
    let (tx, rx) = std::sync::mpsc::channel::<Option<u64>>();
    for url in URLS {
        let tx = tx.clone();
        let u = format!("{url}?cb={cb}");
        std::thread::spawn(move || {
            let h = (|| -> Option<u64> {
                // 0.77: shared pooled client — was a fresh Client per racer thread per poll.
                let v = HTTP_BLOCKING.get(&u).header("cache-control", "no-cache").send().ok()?
                    .json::<serde_json::Value>().ok()?;
                v.get("height").and_then(|x| x.as_u64()).filter(|&h| h > 0)
            })();
            let _ = tx.send(h); // recv may already be gone (a faster oracle won) — harmless
        });
    }
    drop(tx); // so rx disconnects once both worker threads have answered
    // First positive answer wins the race; otherwise drain until both report (or 6 s safety).
    loop {
        match rx.recv_timeout(Duration::from_secs(6)) {
            Ok(Some(h)) => return Some(h),
            Ok(None) => continue,            // one oracle failed — keep waiting for the other
            Err(_) => return None,           // both failed / disconnected
        }
    }
}

// ── v0.32.5: persisted tip — OFFLINE-RESILIENT COLD START ───────────────────────────────────
// The fast-snap needs a known network tip in peer_best. The eager-seed + poller fetch it from the
// CDN oracles, but if BOTH are unreachable at boot (laptop offline, CDN outage, captive portal),
// peer_best stays 0 and the monitor sits at "connecting…". Cache the last-known tip on disk each
// time it advances; on the next cold start, seed peer_best from it so the snap can STILL fire to a
// recent window. The live poller corrects it upward the instant an oracle answers. Only ever RAISES.
fn tip_cache_path() -> std::path::PathBuf {
    let dir = std::env::var("SIGIL_TOP_HOME").ok().map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::Path::new(&h).join(".sigil-top")))
        .unwrap_or_else(std::env::temp_dir);
    dir.join("last-tip")
}
fn read_persisted_tip() -> Option<u64> {
    std::fs::read_to_string(tip_cache_path()).ok()?.trim().parse::<u64>().ok().filter(|&h| h > 0)
}
fn persist_tip(h: u64) {
    let p = tip_cache_path();
    if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(p, h.to_string());
}
/// v0.36.1: drop the persisted tip so a restart doesn't re-seed a stale (pre-reset)
/// height. Called when chain-reset detection fires in the tip-poller.
fn clear_persisted_tip() { let _ = std::fs::remove_file(tip_cache_path()); }

async fn fetch_live_tip_inner(client: &reqwest::Client) -> Option<u64> {
    const URLS: [&str; 2] = [
        "https://sigilgraph.fluxapp.xyz/sigil-tip-live.json",
        "https://quillon.xyz/sigil-tip-live.json",
    ];
    // cache-buster (per-call) so a CDN/proxy never pins the tip; the publisher uses one too.
    let cb = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    for url in URLS {
        let u = format!("{url}?cb={cb}");
        match client.get(&u).header("cache-control", "no-cache").send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    if let Some(h) = v.get("height").and_then(|x| x.as_u64()) {
                        if h > 0 { return Some(h); }
                    }
                }
                Err(e) => crate::tlog!("[tip] {url} json err: {e}"),
            },
            Err(e) => crate::tlog!("[tip] {url} get err: {e}"),
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
pub struct P2PSyncState {
    pub running: bool,
    pub peer_count: u32,
    pub mesh_peer_count: u32,
    pub peer_best_height: u64,
    pub blocks_synced: u64,
    pub last_message_at: Option<Instant>,
    /// v0.26: when the tip-poller last got a fresh tip. UI shows STALE if this ages out
    /// (oracle down / network partition) instead of a falsely confident "AT TIP".
    pub last_tip_at: Option<Instant>,
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
    /// v0.22.26: the recent-window anchor (blocks below it are NOT downloaded — they are
    /// attested by the fold-proof, not synced). `blocks_synced - base` = blocks REALLY
    /// downloaded in the live window. Lets the UI tell the truth instead of reporting the
    /// base-jumped `synced_to` as if the whole chain were downloaded.
    pub base: u64,
    /// LANE-S: set by the tip-poller when it detects a chain reset (the live oracle tip is
    /// drastically below peer_best). The sync loop consumes it on its next tick — wipes the
    /// block-store watermarks (synced_to/verified_to/best) so the stale OLD-genesis chain is
    /// forgotten and re-downloaded from the fresh tip — then clears the flag. This is what
    /// makes a testnet reset self-heal with NO manual local wipe.
    pub reset_pending: bool,
    /// v0.9.0: contiguous CRYPTOGRAPHICALLY-VERIFIED tip — blocks 0..verified each passed
    /// precheck + parent-linkage (spine connects back to genesis). `blocks_synced` means
    /// "downloaded"; THIS means "downloaded AND validated as one chain". The full-sync
    /// completion gate watches this, not blocks_synced.
    pub verified: u64,
    /// v0.9.0: set when the verifier hit a real integrity break (NOT the clean download
    /// frontier): "(height) reason". Empty while the chain is clean. Surfaced in the TUI
    /// + makes `full-sync`/`verify-chain` exit non-zero.
    pub verify_break: Option<String>,
    /// v0.57 (LANE-M): true in RECENT-WINDOW (light monitor) mode — the base is snapped
    /// forward to a recent servable window, so `verified` is anchored at that checkpoint base
    /// (tip-proof semantics), NOT a full spine linked to genesis. The renderer reads this to be
    /// HONEST: track STORED progress on the bar + show `verified` as a separate checkpoint badge,
    /// never a frozen full-genesis-spine %. False in full-sync (--sync genesis) where `verified`
    /// IS the genesis spine. See `chain_verify::verify_to` (walks from `max(verified_to, base)`).
    pub light_mode: bool,
    /// v0.27 PROOF-OF-USEFUL-SYNC: idle-at-tip CPU re-derives the stored spine's BLAKE
    /// hashes (same methodology as mining) to harden chain trust instead of idling.
    /// Cumulative headers re-verified this session + the rolling rate (useful hashrate).
    pub pos_total: u64,
    pub pos_rate: f64,
    /// LANE-P v0.59: HONEST stall surfacing — non-empty when the contiguous frontier has
    /// not advanced for a while while a higher tip is known. Surfaced in the SYNC hero so a
    /// stall is NEVER a silent 0 blk/s; cleared the moment the frontier advances again.
    pub stall_reason: String,
    /// v0.59: the latest height from the SIGNED sigil-tip-live.json oracle — the network AUTHORITY
    /// for the tip. Gossip-claimed raises are gated against this (see `sane_raise`) so a phantom or
    /// post-genesis-reset gossip can't push the sync target above the real chain head. 0 until the
    /// first oracle answer.
    pub oracle_tip: u64,
    /// SPINE-BREAK fix: a CONFIRMED, operator-visible sync failure = (stuck_height, reason).
    /// Set LOUD by the no-progress watchdog (an unfillable hole at the contiguous frontier
    /// while a higher block is already held) or immediately on a FATAL verify break
    /// (parent-linkage / precheck / corrupt-hash). This is what makes the old "~499k SPINE
    /// BREAK" stall NEVER a silent rate-0 again — `verify_break` only catches corruption,
    /// `Missing` holes used to vanish; this names the EXACT stuck height instead. Distinct
    /// from `stall_reason` (a soft, transient "retrying" hint that self-clears).
    pub sync_failure: Option<(u64, String)>,
}

pub struct P2PBlockSync {
    state: Arc<Mutex<P2PSyncState>>,
    new_blocks: Arc<Mutex<Vec<StoredBlock>>>,
    stop_tx: Option<mpsc::Sender<()>>,
    /// 0.77 GENESIS ARCHIVE: the sync mode is LIVE-FLIPPABLE — [F] toggles a RUNNING
    /// engine between light-monitor (recent-window snap) and full-archive (genesis→tip,
    /// hold everything) with no restart. Every base-snap gate in the engine loads this.
    recent_only: Arc<AtomicBool>,
    /// Set by `set_full_archive`; the engine thread consumes it at tick-top and
    /// re-anchors the store at the genesis base so the frontier re-walks genesis→tip.
    rebase_pending: Arc<AtomicBool>,
}

pub use super::block_store::StoredBlock;

impl P2PBlockSync {
    /// v0.11.0: share the live sync state with the embedded explorer API (serve.rs)
    /// so `/api/v1/{status,peers}` reflect the real mesh height / verified watermark /
    /// peer count instead of being proxied to the remote node.
    pub fn state_handle(&self) -> Arc<Mutex<P2PSyncState>> {
        self.state.clone()
    }

    /// 0.77: non-blocking state access for the DRAW thread. The sync thread holds the
    /// state mutex frequently (once per ingested block + ~15 sites per tick); a blocking
    /// `lock()` from the render loop starved the draw thread under full-archive load on
    /// Windows (unfair SRWLOCK) → BLACK SCREEN at 918MB sync (#156 item 2). `try_lock`
    /// + the caller keeping its last clone = at worst a 1-frame-stale readout.
    fn try_state(&self) -> Option<std::sync::MutexGuard<'_, P2PSyncState>> {
        match self.state.try_lock() {
            Ok(g) => Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// 0.77 GENESIS ARCHIVE: flip a RUNNING engine to full-archive mode — base re-anchors
    /// at genesis on the engine thread's next tick; the frontier re-walks genesis→tip and
    /// HOLDS every block (the redundant-backup promise of [F], #156 item 1).
    pub fn set_full_archive(&self) {
        self.recent_only.store(false, Ordering::Relaxed);
        self.rebase_pending.store(true, Ordering::Relaxed);
    }

    /// 0.77: flip a RUNNING engine back to light-monitor — the recent-window snap gates
    /// re-engage and the base snaps forward to the servable window naturally.
    pub fn set_light_monitor(&self) {
        self.recent_only.store(true, Ordering::Relaxed);
    }

    /// 0.77: the engine's CURRENT mode (true = light-monitor / recent-window).
    pub fn is_recent_only(&self) -> bool {
        self.recent_only.load(Ordering::Relaxed)
    }

    /// v0.13.1: seed the network tip from an EXTERNAL source (the HTTP status feed)
    /// so the backfill refill fires even when gossip AND the P2P height-probe are
    /// silent (a frozen or quiet mesh, or a producer that gossips nothing). Before
    /// this, `peer_best_height` was learnable ONLY from inbound gossip / a probe
    /// reply; on a quiet mesh it stayed 0, the `peer_best > 0` refill gate never
    /// opened, and the sync sat on "connecting" forever. Only ever RAISES the tip.
    /// 0.77: try_lock — called from the draw thread every frame; a skipped hint is
    /// retried next frame, but it must NEVER block render.
    pub fn set_known_tip(&self, height: u64) {
        if height == 0 { return; }
        if let Some(mut s) = self.try_state() {
            if height > s.peer_best_height {
                s.peer_best_height = height;
            }
        }
    }

    /// `recent_only`: a far-behind MONITOR snaps its sync base to a recent window the
    /// producers serve fast (instead of crawling genesis→tip at the rate slow/gappy
    /// historical ranges dribble — the "1 blk/s" symptom). full-sync passes false.
    pub fn launch(mut store: BlockStore, recent_only: bool) -> Self {
        // SIGIL_SNAP=1 forces fast-snap even in full-sync (validation / "just track the tip").
        let recent_only_init = recent_only || std::env::var("SIGIL_SNAP").is_ok();
        // 0.77 GENESIS ARCHIVE: the mode is LIVE-FLIPPABLE ([F] reaches a running engine
        // through this atomic — before 0.77 the bool was captured by value and the toggle
        // was a TUI-local no-op). Every base-snap gate below loads it fresh.
        let recent_only = Arc::new(AtomicBool::new(recent_only_init));
        let rebase_pending = Arc::new(AtomicBool::new(false));
        let recent_only_rt = recent_only.clone();
        let rebase_pending_rt = rebase_pending.clone();
        let state = Arc::new(Mutex::new(P2PSyncState::default()));
        let new_blocks = Arc::new(Mutex::new(Vec::new()));
        let (stop_tx, stop_rx) = mpsc::channel();

        let state_clone = state.clone();
        let new_blocks_clone = new_blocks.clone();

        // v0.21: DEDICATED tip-poller thread. The monitor's fast-snap needs a FRESH live tip
        // in peer_best; the in-runtime async fetch was non-deterministically starved by the
        // backfill/verify workload (peer_best froze → the monitor parked behind the tip at
        // 0 blk/s). A standalone OS thread with BLOCKING reqwest polls every 3s, immune to
        // that contention, and seeds peer_best directly.
        // 0.77: the dedicated tip-poller spawn keeps gating on the LAUNCH-TIME mode (a
        // full-archive launch flipped to light later still gets tips from the in-loop
        // async fetch — just without the dedicated anti-starvation thread; acceptable).
        if recent_only_init {
            // v0.35 (sync-starts-earlier, DeepSeek audit S2): the v0.23 SYNCHRONOUS CDN
            // eager-seed is GONE — it blocked launch() for up to ~6 s of HTTP before the
            // sync loop could even spawn, serializing exactly the startup it meant to speed
            // up. The poller thread below fires its first fetch IMMEDIATELY on spawn (fetch
            // precedes the first sleep), so the CDN tip still lands within one RTT — now in
            // parallel with the mesh bootstrap instead of ahead of it.
            // v0.32.5: OFFLINE-RESILIENT instant seed — the LAST-KNOWN tip persisted on a
            // prior run (one disk read, microseconds) so the fast-snap can fire on cycle 1
            // even before any oracle answers / fully offline. The poller corrects it upward
            // the moment a CDN answers. Only ever raises peer_best.
            if let Some(h) = read_persisted_tip() {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                if h > s.peer_best_height { s.peer_best_height = h; }
            }
            // v0.21: DEDICATED tip-poller thread. The monitor's fast-snap needs a FRESH live
            // tip in peer_best; the in-runtime async fetch was non-deterministically starved by
            // the backfill/verify workload (peer_best froze → monitor parked behind the tip at
            // 0 blk/s). A standalone OS thread with BLOCKING reqwest is immune to that
            // contention and seeds peer_best directly.
            // v0.23: ADAPTIVE cadence — poll every 1s during warmup (first ~15 polls) so a
            // freshly-launched monitor pins to a fast-moving tip immediately, then settle to
            // 3s steady-state (the tip moves ~100 blk/s, so 3s is ample once caught up).
            let tip_state = state.clone();
            thread::spawn(move || {
                let mut polls: u32 = 0;
                let mut reset_streak: u32 = 0; // v0.36.1 chain-reset detection
                let mut fail_backoff = Duration::from_secs(0);
                loop {
                    match fetch_live_tip_blocking() {
                        Some(h) => {
                            fail_backoff = Duration::from_secs(0); // healthy → normal cadence
                            // v0.36.1 CHAIN-RESET DETECTION: the oracle is the network source of
                            // truth. peer_best only ever RAISES (offline-resilience), so after a
                            // testnet reset a stale high (e.g. 21.9M) sticks forever and the UI
                            // shows a phantom tip. If the live oracle reports a tip DRASTICALLY
                            // below peer_best for 3 consecutive polls (~9s — not a transient dip),
                            // the chain was reset: adopt the oracle value + clear the persisted
                            // last-tip so a restart doesn't re-poison from disk.
                            let mut persist: Option<u64> = None;
                            {
                                let mut s = tip_state.lock().unwrap_or_else(|e| e.into_inner());
                                s.last_tip_at = Some(Instant::now());
                                let pb = s.peer_best_height;
                                if pb > 0 && h < pb / 2 && pb - h > 100_000 {
                                    reset_streak += 1;
                                    // LANE-S: fire IMMEDIATELY on an UNAMBIGUOUS reset (live tip < ¼
                                    // of peer_best — a 4×+ drop is never a transient dip). The old
                                    // 3-poll (~9s) wait is what left the phantom checkpoint (5M while
                                    // tip was 394k = 13× below) up for "a LONG time". A milder ¼..½
                                    // drop still needs the 3-poll confirm to avoid flapping.
                                    if reset_streak >= 3 || h < pb / 4 {
                                        s.peer_best_height = h; // RESET to the live oracle tip
                                        // a chain reset invalidates the checkpoint/spine high-water
                                        // marks — they were verified against the OLD (now-dead) chain.
                                        s.blocks_synced = s.blocks_synced.min(h);
                                        s.sync_height   = s.sync_height.min(h);
                                        s.sync_total    = s.sync_total.min(h);
                                        s.verified      = s.verified.min(h);
                                        if s.base > h { s.base = h; }
                                        s.reset_pending = true; // tell the sync loop to wipe the store
                                        reset_streak = 0;
                                        clear_persisted_tip();
                                        persist = Some(h);
                                    }
                                } else {
                                    reset_streak = 0;
                                    s.oracle_tip = h; // the signed oracle anchor — gates gossip raises
                                    if h > s.peer_best_height {
                                        s.peer_best_height = h; persist = Some(h);
                                    } else if s.peer_best_height > h.saturating_add(SANE_LEAD) {
                                        // v0.59 ORACLE-AUTHORITATIVE: peer_best drifted ABOVE the signed
                                        // oracle by more than a sane lead — a phantom gossip claim (the
                                        // 1.4M post-genesis-reset ghost) that the drastic-drop branch
                                        // above MISSES (it isn't < pb/2). The signed oracle wins: snap
                                        // peer_best back to it + clamp the progress watermarks so the UI
                                        // can't chase a tip that doesn't exist, and clear the persisted
                                        // seed so a restart doesn't re-poison from disk.
                                        s.peer_best_height = h;
                                        s.blocks_synced = s.blocks_synced.min(h);
                                        s.sync_height   = s.sync_height.min(h);
                                        s.sync_total    = s.sync_total.min(h);
                                        s.verified      = s.verified.min(h);
                                        if s.base > h { s.base = h; }
                                        s.reset_pending = true; // LANE-S: wipe the store too
                                        clear_persisted_tip();
                                        persist = Some(h);
                                    }
                                }
                            }
                            if let Some(h) = persist { persist_tip(h); }
                        }
                        None => {
                            // v0.26: exponential backoff (cap 60s) on repeated oracle failure so we
                            // don't hammer a dead endpoint; the UI surfaces STALE via last_tip_at.
                            fail_backoff = (fail_backoff.max(Duration::from_secs(2)) * 2).min(Duration::from_secs(60));
                        }
                    }
                    polls = polls.saturating_add(1);
                    let base = if polls < 15 { Duration::from_millis(500) } else { Duration::from_millis(800) };
                    thread::sleep(base.max(fail_backoff));
                }
            });
        }

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
                    let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                    s.running = true;
                    s.blocks_synced = resume_h;
                    s.verified = store.verified_to(); // v0.9.0: resume the verified watermark too
                    s.light_mode = recent_only_rt.load(Ordering::Relaxed); // v0.57 (LANE-M): drives honest verified-vs-stored UI
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
                // CHUNK = the per-request span AND the look-ahead stride. It must MATCH what
                // the responders actually serve per reply: today they serve ~4096 headers/reply,
                // so a larger CHUNK makes look-ahead chunks land SPARSE (gaps between prefetched
                // ranges) and the contiguous frontier ends up doing all the work serially — the
                // exact regression a 0.56 bump to 32768 caused. Keep the default at the proven
                // 4096; once the fleet responders serve a bigger SIGIL_SERVE_HEADERS_CAP, raise
                // BOTH together via SIGIL_SYNC_CHUNK. The v0.57 frontier-exact fix below makes any
                // value SAFE (partial fills always advance), but 4096 stays OPTIMAL for the live
                // mesh. Env-tunable.
                #[allow(non_snake_case)]
                let CHUNK: u64 = std::env::var("SIGIL_SYNC_CHUNK").ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|n| n.clamp(1024, 65_536)).unwrap_or(4096);
                // v0.39: was const 12 — at first boot (empty DB) all slots fire decode
                // bursts at once, pre-TUI, which pressured small/busy machines hard. 8 by
                // default; SIGIL_SYNC_INFLIGHT=1..16 to tune (raise on a beefy box).
                let max_inflight: usize = std::env::var("SIGIL_SYNC_INFLIGHT").ok()
                    .and_then(|v| v.parse::<usize>().ok()).map(|n| n.clamp(1, 16)).unwrap_or(8);
                                                    // onto a stalled frontier and crater the rate.
                // Look-ahead cap must be TIGHT: a large window lets next_start race far ahead of a
                // stalled frontier, so all MAX_INFLIGHT slots get consumed by high-range chunks that
                // don't advance synced_to while the lead chunk starves (v0.10.0 frontier-stall bug).
                // v0.12.1: was 4s — far too tight. The producers serve backfill while
                // also producing ~100 blk/s (≈54% CPU), so a ~2 MB chunk over WAN
                // routinely takes >4s → EVERY request timed out → fetched_total stuck at
                // 0 → the UI sat at "connecting…" forever. 15s lets a slow-but-alive
                // serve complete; dead peers are still benched on timeout and rerouted.
                const REQ_TIMEOUT: Duration = Duration::from_secs(10); // v0.15.1: 15→10s — free a stuck slot faster (4 MB chunk lands well under 10s); still tolerant of slow WAN serves
                const PROBE_EVERY: Duration = Duration::from_millis(500); // pull-height probe cadence
                const BENCH: Duration = Duration::from_secs(4); // v0.15.1: 8→4s — faster peer recovery so the pool doesn't thin out
                const EMPTY_BENCH: Duration = Duration::from_secs(10); // v0.15.1: 45→10s — THE stall fix: 45s drained the 4-peer pool to ~0 on empty ranges → 2 blk/s. 10s rotates away yet keeps peers available.
                let _ = resume_h; // frontier is read live from the store each cycle (anchored, not cursored)
                // Completed request results flow back from the spawned tasks here.
                let (done_tx, mut done_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(u64, String, Option<Vec<u8>>)>();
                // pull HEIGHT-PROBE replies (open-ended range → peer's clamped tip)
                let (probe_tx, mut probe_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                // v0.17.0: the TRUE network tip from the published sigil-tip-live.json (the
                // /api/v1/status the monitor polls returns height=2 → snap never fired).
                let (tip_tx, mut tip_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
                let mut last_tip_fetch = crate::instant_ago(60);
                // v0.50 (LANE-A): RECENT-WINDOW PROBE-BEFORE-SNAP. A monitor resumed far below the
                // tip crawls the middle history it doesn't need (the fold-proof attests it) because
                // the fast-snap is gated on best_height (received), which only crawls forward at the
                // backfill rate. This probes ONE chunk at peer_best-RECENT directly; a NON-EMPTY
                // reply PROVES the reachable peers serve the recent window, so we snap the base there
                // and reach the tip in seconds. An EMPTY reply (peers behind the oracle tip) costs
                // one request and changes nothing — so this can NEVER trigger the v0.16 "snap to an
                // unservable tip → 0 downloaded" regression (the reason the snap is best_height-gated).
                let (recent_tx, mut recent_rx) =
                    tokio::sync::mpsc::unbounded_channel::<(u64, Option<Vec<u8>>)>();
                let mut last_recent_probe = crate::instant_ago(60);
                let mut recent_probe_inflight = false;
                let mut inflight: usize = 0;                          // outstanding spawned requests
                let mut assigned: std::collections::HashSet<u64> = std::collections::HashSet::new();
                let mut peer_bench: HashMap<String, Instant> = HashMap::new(); // peer.to_string() → benched-until
                // v0.31.6: per-peer KNOWN TOP height (max height it has served). Lets the refill
                // skip peers that are BEHIND the frontier — they'd just return EMPTY (the "producers
                // serving empty for the head" symptom). Updated from every response.
                let mut peer_top: HashMap<String, (u64, Instant)> = HashMap::new();
                let mut rr: usize = 0;                                 // round-robin peer cursor
                let mut last_state = crate::instant_ago(1);
                let mut fetched_session: u64 = 0;                      // headers stored this session
                let mut last_verify = crate::instant_ago(2); // slow verify+flush timer
                let mut last_synced_seen: u64 = resume_h;             // dynamic-base detector
                let mut last_advance_t = Instant::now();
                // SPINE-BREAK fix: VERIFIED-watermark watchdog. Tracks the last verified_to and
                // when it last advanced; if it parks while a higher block is already held (a real
                // hole), the shared `gap_sync::watchdog_verdict` declares a LOUD failure naming the
                // exact stuck height — never the old silent rate-0 at ~499k.
                let mut last_verified_seen: u64 = store.verified_to();
                let mut last_verified_advance_t = Instant::now();
                // How long the verified frontier may park (with a higher block held) before the
                // watchdog fires loud. Generous enough for a slow WAN serve; env-tunable.
                let watchdog_secs: u64 = std::env::var("SIGIL_SYNC_WATCHDOG_SECS").ok()
                    .and_then(|s| s.parse().ok()).map(|n: u64| n.clamp(5, 600)).unwrap_or(45);
                // One loud log per stall EPISODE (not every verify tick): true once announced,
                // reset when the frontier recovers so a later stall announces again.
                let mut failure_announced = false;
                // v0.31 DEEP DEBUG: session counters + a periodic comprehensive [DBG] snapshot.
                let (mut lead_n, mut timeout_n, mut empty_n, mut req_n): (u64, u64, u64, u64) = (0, 0, 0, 0);
                let mut bytes_session: u64 = 0;
                let mut last_dbg = crate::instant_ago(5);
                let loop_start = Instant::now();
                // v0.27 proof-of-useful-sync local accumulators
                let mut pos_cursor: u64 = 0;
                let mut pos_acc: u64 = 0;
                let mut pos_total_session: u64 = 0;
                let mut pos_t = Instant::now();
                // v0.28 batched useful-sync: cache the window once + gossip a checkpoint
                let mut pos_window: Vec<sigil_header::SigilBlockHeaderV0> = Vec::new();
                let mut pos_window_base: u64 = 0;
                let mut ckpt_t = Instant::now();
                let mut pos_bytes: Vec<u8> = Vec::new(); // v0.29.5 cached window-digest buffer for SIMD blake3
                let mut last_probe = crate::instant_ago(10); // pull-height probe timer
                // v0.15.2: far-behind monitor snaps to a recent window once peer_best is known.
                const RECENT_WINDOW: u64 = 2_048;  // v0.21: pin the base just 1 chunk under the live tip
                let mut snapped = false;

                loop {
                    if stop_rx.try_recv().is_ok() {
                        let _ = net.stop().await;
                        break;
                    }

                    // LANE-S CHAIN-RESET SELF-HEAL: the tip-poller flagged a reset (the live tip
                    // is drastically below our peer_best — a fresh genesis). Wipe the block-store
                    // watermarks (synced_to/verified_to/best) so the stale OLD chain is forgotten
                    // and re-downloaded from the fresh tip, and reset the local cursors so the
                    // refill restarts cleanly. NO manual local wipe needed.
                    {
                        let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                        if s.reset_pending {
                            s.reset_pending = false;
                            drop(s);
                            store.reset_watermarks();
                            assigned.clear();
                            last_synced_seen = 0;
                            last_advance_t = Instant::now();
                            snapped = false;
                            crate::tlog!("[sync] CHAIN-RESET self-heal — block store watermarks wiped, re-syncing from the fresh genesis");
                        }
                    }

                    // ── 0.77 GENESIS ARCHIVE: [F] flipped a RUNNING engine to full-archive.
                    // Re-anchor the store at the genesis base so the contiguous frontier
                    // re-walks genesis→tip and HOLDS every block (#156: the operator's
                    // Windows PC as a redundant full archive if the mine-node fleet is lost).
                    // The recent-window blocks already on disk stay — the frontier absorbs
                    // them as out-of-order arrivals when it reaches them.
                    if rebase_pending_rt.swap(false, Ordering::Relaxed) {
                        store.rebase(sync_base);
                        assigned.clear();      // stale recent-window frontier reqs are useless now
                        snapped = false;
                        last_synced_seen = store.synced_to();
                        last_advance_t = Instant::now();
                        crate::tlog!("[sync] FULL ARCHIVE engaged — base → {} (frontier re-walks genesis→tip, holding every block)", sync_base);
                    }

                    // Process gossiped live-tip blocks — BOUNDED per iteration. The live mesh
                    // (incl. our local catching-up node) floods this topic; an UNBOUNDED drain
                    // here blocks the loop for seconds (each block costs a serde_json hash),
                    // which starved the whole pipeline (v0.10.0 synced-stuck bug). Cap it so the
                    // loop stays responsive; leftover messages drain over the next iterations.
                    // v0.50 (LANE-A · fix-3 REAL-TIME GOSSIP HEAD): split the gossip drain into
                    // a CHEAP head-scan + a BOUNDED full-ingest. Before this, the tip (peer_best)
                    // effectively advanced only at the 1-3 s ORACLE cadence (`[tipfetch]`): the
                    // gossip drain that could advance it was capped at 48/iter, so under bulk-sync
                    // load the live head blocks queued behind the cap and the hero gap tracked the
                    // oracle's staleness, not the real tip. Now EVERY pending gossip block (up to
                    // HEAD_SCAN_CAP) cheaply contributes its height to `head_seen` → peer_best is
                    // raised the MOMENT a block gossips in (sub-second, gossip-driven, independent
                    // of the oracle). The EXPENSIVE work (hash + store + hand-off, the per-block
                    // serde+blake the v0.10.0 synced-stuck bug warns about) stays BOUNDED at
                    // INGEST_CAP so the loop never blocks for seconds under a re-gossip flood;
                    // leftover blocks ingest over later iterations and the backfill fills any
                    // contiguity gap. No new polling — pure event drain, poll budget unchanged.
                    const HEAD_SCAN_CAP: u32 = 512;  // cheap height-peek bound per iter (flood-proof head)
                    const INGEST_CAP: u32 = 48;      // expensive store bound (v0.25.5 value, unchanged)
                    let mut gdrained = 0u32;
                    let mut ingested = 0u32;
                    let mut head_seen: u64 = 0;
                    while gdrained < HEAD_SCAN_CAP {
                        let (_topic, data) = match block_rx.try_recv() { Ok(x) => x, Err(_) => break };
                        gdrained += 1;
                        let v: serde_json::Value = match serde_json::from_slice(&data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if v.get("sync_from").is_some() { continue; }
                        // Cheap: the live tip is the MAX gossiped height. Peek it without the
                        // costly header.hash() so the head advances even past the ingest cap.
                        let h = v.get("header").and_then(|x| x.get("height")).and_then(|x| x.as_u64())
                            .or_else(|| v.get("header_json").and_then(|x| x.as_str())
                                .and_then(|hj| serde_json::from_str::<SigilBlockHeaderV0>(hj).ok())
                                .map(|hdr| hdr.height));
                        if let Some(h) = h { if h > head_seen { head_seen = h; } }
                        // Expensive: store + advance the contiguous frontier — bounded per iter.
                        if ingested < INGEST_CAP {
                            ingested += 1;
                            ingest_block_value(&v, &mut store, &state_clone, &net, &new_blocks_clone);
                        }
                    }
                    // Advance the live tip from gossip immediately (gossip is proof the network is
                    // AT LEAST at head_seen). sane_raise still vetoes phantom jumps (>2M past belief
                    // stay the oracle's call). Stamp last_tip_at so the head stays FRESH off gossip
                    // alone — the hero no longer reads STALE / parks behind a 1-3 s oracle poll.
                    if head_seen > 0 {
                        let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                        if head_seen > s.peer_best_height && sane_raise(s.oracle_tip, s.peer_best_height, head_seen) {
                            s.peer_best_height = head_seen;
                            s.last_message_at = Some(Instant::now());
                            s.last_tip_at = Some(Instant::now());
                        }
                    }

                    // Peer events from drain_events (non-block messages)
                    for event in net.drain_events() {
                        match event {
                            flux_p2p::SwarmAppEvent::PeerConnected { peer_id, addr } => {
                                let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                let pc = net.peer_count();
                                let prev = s.peer_count;
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
                                // v0.31 DETAILED MESH DEBUG: name the fleet node by IP + show the
                                // peer-count transition, so "caps at 3/4 peers" is diagnosable from
                                // the log — you see exactly which node joins and which never does.
                                let node = if addr.contains("89.149.241.126") { "epsilon" }
                                    else if addr.contains("5.79.79.158") { "delta" }
                                    else if addr.contains("109.205.176.60") { "gamma" }
                                    else if addr.contains("185.182.185.227") { "beta" }
                                    else { "peer" };
                                crate::tlog!("[mesh] ＋CONNECT {node:<8} {peer_id} @ {addr}   peers {prev}→{pc}");
                            }
                            flux_p2p::SwarmAppEvent::PeerDisconnected { peer_id } => {
                                let pc = net.peer_count();
                                let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                let prev = s.peer_count;
                                s.peer_count = pc;
                                s.mesh_peer_count = pc;
                                if pc == 0 {
                                    s.connected_delta = false;
                                    s.connected_epsilon = false;
                                }
                                // v0.31 DETAILED MESH DEBUG: which peer dropped + the new count. A
                                // peer that repeatedly CONNECTs then DROPs is the rotating-identity
                                // signature (the bug the stable peer-id in for_sigil v0.31 fixes).
                                crate::tlog!("[mesh] －DROP    {peer_id}   peers {prev}→{pc}{}",
                                    if pc == 0 { "   ⚠ MESH EMPTY — no peers" } else { "" });
                            }
                            flux_p2p::SwarmAppEvent::SyncProgress { height, hash_hex, peer_best_height, total_synced, peer_count: _ } => {
                                let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                // v0.18.5: the gossiped `height` is the NETWORK TIP, not our sync
                                // progress. Do NOT clobber sync_height with it (that made the
                                // dashboard show the ~6.9M tip as synced_to and a 0 blk/s rate while
                                // the real backfill frontier climbed underneath). peer_best (the
                                // target) is updated below; sync_height stays the real frontier set
                                // in the fast-periodic from store.synced_to().
                                let _gossiped_tip = height;
                                s.sync_hash_hex = hash_hex;
                                s.sync_total = total_synced;
                                if peer_best_height > s.peer_best_height
                                    && sane_raise(s.oracle_tip, s.peer_best_height, peer_best_height) {
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
                                bytes_session += b.len() as u64;
                                if got > 0 { lead_n += 1; } else { empty_n += 1; }
                                // v0.31.6: learn this peer's TOP. LEAD → its served max height;
                                // EMPTY at the frontier → it's BEHIND `start`, so cap its known top
                                // just below start. The refill uses this to stop sending the head to
                                // peers that would only answer EMPTY (the "serving empty for the head").
                                if got > 0 {
                                    if let Some(mx) = max_header_height(&b) {
                                        peer_top.insert(peer.clone(), (mx, Instant::now()));
                                    }
                                } else if start >= (store.synced_to() / CHUNK) * CHUNK {
                                    // behind the frontier — record a fresh "top < start" marker
                                    peer_top.insert(peer.clone(), (start.saturating_sub(1), Instant::now()));
                                }
                                if start <= store.synced_to() + CHUNK {
                                    // v0.38.1: decode the height range across ALL codecs ('Z' zstd /
                                    // 'H' bincode / JSON). The old line only handled 'H', so since the
                                    // codec=1 zstd lane went live every chunk logged h=[0..0] — a pure
                                    // display bug that made a healthy sync look broken ("0.38 doesn't sync").
                                    let (mn,mx) = header_height_range(&b).unwrap_or((0,0));
                                    crate::tlog!("[D] {} start={start} got={got} h=[{mn}..{mx}] bytes={} synced={} inflight={inflight}", if got>0 {"LEAD"} else {"EMPTY"}, b.len(), store.synced_to());
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
                                timeout_n += 1;
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
                            let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
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
                        let now = Instant::now();
                        let mut healthy: Vec<_> = net.connected_peers().into_iter()
                            .filter(|p| peer_bench.get(&p.to_string()).map_or(true, |&u| now >= u))
                            .collect();
                        // v0.17: HEALTHY-PEER FLOOR. With only ~4 peers a burst of timeouts
                        // could bench the WHOLE pool, collapsing the backfill to ~0 blk/s until
                        // benches expired — the erratic 57k/77k/127k progress. Never stall while
                        // peers are connected: if every peer is benched, fall back to the full
                        // set so the probe/refill keeps firing best-effort (bench is advisory).
                        if healthy.is_empty() { healthy = net.connected_peers(); }
                        // v0.38.1: prefer a peer NOT known to be behind the frontier for the
                        // open-ended probe. A behind peer answers EMPTY for [frontier, MAX], so
                        // probing it wastes a round-trip AND seeds nothing useful into peer_best.
                        // Pick a peer whose known top is at/above the frontier (or unknown — give
                        // it a chance); fall back to healthy.first() so we never skip a probe.
                        let probe_peer = {
                            let fc = frontier_chunk;
                            healthy.iter().find(|p| match peer_top.get(&p.to_string()) {
                                Some(&(top, seen)) => top + CHUNK >= fc || now.duration_since(seen).as_secs() > 4,
                                None => true,
                            }).or_else(|| healthy.first()).copied()
                        };
                        if let Some(peer) = probe_peer {
                            // v0.35 (DeepSeek audit S5): stamp the timer ONLY when a probe is
                            // actually SENT. It used to be stamped before the peer check, so
                            // with 0 peers connected (the first loop ticks, pre-bootstrap) the
                            // overdue first probe was BURNED and the real first probe waited an
                            // extra PROBE_EVERY after the first PeerConnected. Now the probe
                            // fires on the very next 10ms tick after a peer lands.
                            last_probe = Instant::now();
                            // LANE-P v0.59 STALL-BREAKER: normally probe the floor-aligned
                            // frontier_chunk (cache-friendly look-ahead). But if the contiguous
                            // frontier hasn't advanced for a while, request the EXACT next-needed
                            // height [synced_to..] from this (rotating) healthy peer — bypasses any
                            // residual floor-alignment edge so the lead block lands and synced_to moves.
                            let probe_from = if last_advance_t.elapsed() >= Duration::from_secs(6) {
                                store.synced_to()
                            } else {
                                frontier_chunk
                            };
                            let payload = serde_json::to_vec(
                                &BackfillReq { from: probe_from, to: u64::MAX, headers_only: true, codec: 1 }
                            ).unwrap();
                            let n = net.clone();
                            let tx = probe_tx.clone();
                            tokio::spawn(async move {
                                let r = tokio::time::timeout(REQ_TIMEOUT, n.send_request(peer, payload)).await;
                                if let Ok(Ok(b)) = r { let _ = tx.send(b); }
                            });
                        }
                    }

                    // v0.17.0: refresh the TRUE tip from sigil-tip-live.json every 3s and seed
                    // peer_best — the reliable signal that makes the fast-snap actually fire
                    // (the /api/v1/status the monitor reads returns height=2). Spawned so the
                    // HTTP round-trip never blocks the sync loop; result drained next tick.
                    while let Ok(h) = tip_rx.try_recv() {
                        let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                        let old = s.peer_best_height;
                        if h > s.peer_best_height { s.peer_best_height = h; }
                        crate::tlog!("[tipfetch] got {} (peer_best {} -> {})", h, old, s.peer_best_height);
                    }
                    if last_tip_fetch.elapsed() >= Duration::from_secs(3) {
                        last_tip_fetch = Instant::now();
                        let tx = tip_tx.clone();
                        tokio::spawn(async move {
                            if let Some(h) = fetch_live_tip().await { let _ = tx.send(h); }
                        });
                    }

                    let peer_best = state_clone.lock().unwrap_or_else(|e| e.into_inner()).peer_best_height;

                    // ── v0.50 RECENT-WINDOW PROBE-BEFORE-SNAP (monitor fast-track) ───────────────
                    // Drain last cycle's probe reply first. got>0 = peers SERVE the recent window →
                    // snap the base there (reach the tip in seconds); got==0 = peers are behind the
                    // oracle tip → hold the contiguous crawl (the SAFE branch, no regression).
                    while let Ok((rbase, bytes)) = recent_rx.try_recv() {
                        recent_probe_inflight = false;
                        if let Some(b) = bytes {
                            let got = ingest_backfill_bytes(&b, &mut store); // free headers near the tip
                            fetched_session += got as u64;
                            if got > 0 && recent_only_rt.load(Ordering::Relaxed) && rbase > store.synced_to() {
                                let old = store.synced_to();
                                store.set_base(rbase);
                                store.advance();
                                assigned.clear(); // stale frontier reqs are useless after the jump
                                last_synced_seen = store.synced_to();
                                last_advance_t = Instant::now();
                                snapped = true;
                                crate::tlog!("[sync] RECENT-PROBE hit: peers serve [{}..] (got {}) — snap base {} → {} (tip {})",
                                    rbase, got, old, store.synced_to(), peer_best);
                            } else if got == 0 {
                                crate::tlog!("[sync] RECENT-PROBE miss at {}: peers behind oracle tip {} — hold crawl", rbase, peer_best);
                            }
                        }
                    }
                    // Send a new probe when a MONITOR is meaningfully behind: confirm whether the
                    // recent window is servable before committing to a snap. Cheap (one request /3s).
                    const RECENT_PROBE_EVERY: Duration = Duration::from_secs(3);
                    // Only fast-track when MEANINGFULLY behind (~5 min of production), so normal
                    // tip-tracking jitter (gap < a few k, held tight by the LANE-A gossip head)
                    // never probes — only a real fall-behind triggers the snap attempt.
                    const RECENT_PROBE_MIN_GAP: u64 = 65_536;
                    if recent_only_rt.load(Ordering::Relaxed)
                        && peer_best > store.synced_to().saturating_add(RECENT_PROBE_MIN_GAP)
                        && !recent_probe_inflight
                        && last_recent_probe.elapsed() >= RECENT_PROBE_EVERY
                    {
                        let now = Instant::now();
                        let mut healthy: Vec<_> = net.connected_peers().into_iter()
                            .filter(|p| peer_bench.get(&p.to_string()).map_or(true, |&u| now >= u))
                            .collect();
                        if healthy.is_empty() { healthy = net.connected_peers(); }
                        if let Some(&peer) = healthy.first() {
                            last_recent_probe = Instant::now();
                            recent_probe_inflight = true;
                            // RECENT_WINDOW < CHUNK, so this chunk straddles the tip; the responder
                            // clamps [from, min(to, its_tip)] and returns the recent headers it has.
                            let rbase = align_base(peer_best.saturating_sub(RECENT_WINDOW), CHUNK, sync_base);
                            let payload = serde_json::to_vec(
                                &BackfillReq { from: rbase, to: rbase + CHUNK, headers_only: true, codec: 1 }
                            ).unwrap();
                            let n = net.clone();
                            let tx = recent_tx.clone();
                            tokio::spawn(async move {
                                let r = tokio::time::timeout(REQ_TIMEOUT, n.send_request(peer, payload)).await;
                                let bytes = match r { Ok(Ok(b)) => Some(b), _ => None };
                                let _ = tx.send((rbase, bytes));
                            });
                        }
                    }

                    // v0.15.2: MONITOR FAST-SNAP. A monitor that's hundreds of thousands of
                    // blocks behind gains nothing from crawling genesis→tip at the rate the
                    // slow/gappy historical ranges dribble (the "1 blk/s" symptom). Once we
                    // know a peer's tip, jump the base to a recent window the producers serve
                    // fast — the monitor reaches the live tip in seconds. Full chain integrity
                    // is the fold-proof's job, not a 6M-block contiguous download. (full-sync
                    // passes recent_only=false and keeps the genesis-anchored crawl.)
                    // v0.21: CONTINUOUSLY re-snap to chase the live tip. A one-shot snap parked
                    // ~10k under the tip on a gappy final range and never recovered. Re-snap
                    // whenever synced falls RESNAP_GAP behind the tip so the monitor jumps the
                    // gap and stays pinned at the head. (full-sync keeps recent_only=false.)
                    // v0.21: drive the base to (live tip − RECENT_WINDOW) EVERY tick — monotonic
                    // (`new_base > base`). No threshold/one-shot to get stuck on: as long as the
                    // tip-fetch keeps peer_best fresh, the base (and synced) chase the head and the
                    // monitor never parks behind. (full-sync keeps recent_only=false.)
                    // v0.27: re-snap only when we've fallen a MEANINGFUL amount behind the tip,
                    // not every poll — and do NOT clear `assigned`. The old per-poll snap +
                    // assigned.clear() re-issued the whole frontier window every 1-3s, which on a
                    // lossy network (the user's box) became a request/timeout STORM (and churned
                    // memory with the growing in-flight responses). The displayed sync height is
                    // peer_best regardless of base, so the backfill base only needs to stay in the
                    // servable recent window — coarse re-snaps are plenty. Stale below-base chunks
                    // still in flight are simply ignored by the store on arrival (height < base).
                    // FIX: snap to the SERVED top (best_height = max block actually RECEIVED via
                    // backfill/probe), NOT the gossip peer_best. The bootstrap nodes are BEHIND the
                    // gossip tip and return EMPTY (got=0) for it, so snapping to peer_best requested
                    // unservable ranges → 0 downloaded. best_height tracks what the mesh actually
                    // serves, keeping the window in the servable range so downloaded climbs.
                    let served_top = store.best_height();
                    if recent_only_rt.load(Ordering::Relaxed) && served_top > store.synced_to() + RECENT_WINDOW {
                        // v0.57 FRONTIER-STALL FIX: align the recent-window base to a CHUNK boundary.
                        // An UNALIGNED base (e.g. 20481) made the floor-aligned refill request ranges
                        // offset from where the server windows them, leaving a permanent 1-block hole
                        // at base+k*CHUNK (synced_to froze at 57345 = 20481 + 9*4096 — proven live).
                        // align_base() snaps it down to a CHUNK multiple so frontier chunks line up.
                        let new_base = align_base(served_top.saturating_sub(RECENT_WINDOW), CHUNK, sync_base);
                        if new_base > store.base() {
                            store.set_base(new_base);
                            store.advance();
                            last_synced_seen = store.synced_to();
                            last_advance_t = Instant::now();
                            snapped = true;
                            crate::tlog!("[sync] track SERVED top {} → base {} (synced {}, gossip tip {})", served_top, new_base, store.synced_to(), peer_best);
                        }
                    }

                    // v0.21.1 FIX (0 blk/s after snap): `frontier_chunk` was computed at the TOP of
                    // the loop from synced_to() BEFORE the snap above moved the base. So once a snap
                    // fired, the refill below kept requesting GENESIS-area chunks (start=1,4097,…) —
                    // blocks BELOW the new base that ingest but never advance synced_to → the monitor
                    // displayed at the base yet sat at 0.0% / 0 blk/s forever. Recompute the frontier
                    // from the CURRENT (post-snap) synced_to so the refill targets the snapped window.
                    let frontier_chunk = ((store.synced_to() / CHUNK) * CHUNK).max(sync_base);

                    if peer_best > 0 {
                        let now = Instant::now();
                        let mut healthy: Vec<_> = net.connected_peers().into_iter()
                            .filter(|p| peer_bench.get(&p.to_string()).map_or(true, |&u| now >= u))
                            .collect();
                        // v0.17: HEALTHY-PEER FLOOR. With only ~4 peers a burst of timeouts
                        // could bench the WHOLE pool, collapsing the backfill to ~0 blk/s until
                        // benches expired — the erratic 57k/77k/127k progress. Never stall while
                        // peers are connected: if every peer is benched, fall back to the full
                        // set so the probe/refill keeps firing best-effort (bench is advisory).
                        if healthy.is_empty() { healthy = net.connected_peers(); }
                        // v0.31.6: drop peers KNOWN to be behind the frontier — they only answer
                        // EMPTY for the head (the "producers serving empty for the head" symptom),
                        // which also wasted redundancy slots and benched good peers. Keep peers with
                        // an UNKNOWN top (give them a chance) and those at/near the frontier. Fall
                        // back to the full set if this empties it (never stall pre-probe).
                        {
                            let fc = frontier_chunk;
                            let now = Instant::now();
                            let caught: Vec<_> = healthy.iter().cloned()
                                .filter(|p| match peer_top.get(&p.to_string()) {
                                    // exclude ONLY if we recently (≤4s) saw it behind the frontier;
                                    // unknown or stale → include so a caught-up peer gets re-tried.
                                    Some(&(top, seen)) => top + CHUNK >= fc || now.duration_since(seen).as_secs() > 4,
                                    None => true,
                                })
                                .collect();
                            // Only adopt the filtered set if it still has enough peers for
                            // redundancy — else excluding behind peers just concentrates load on
                            // 1 peer and CAUSES timeouts (worse than the occasional empty).
                            if caught.len() >= 2 { healthy = caught; }
                        }
                        if !healthy.is_empty() {
                            // v0.29 (chronos-driven): the FRONTIER chunk (i==0) is the ONE that
                            // advances `synced`. On a lossy network a single peer's timeout stalls
                            // it for a whole cycle → the [D] TIMEOUT storm / erratic progress.
                            // chronos showed redundancy lifts lossy delivery 75%→98%, so request the
                            // frontier from up to FRONTIER_REDUNDANCY peers IN PARALLEL — it lands as
                            // soon as ANY responds; duplicate replies are idempotent (store dedups by
                            // height). Look-ahead chunks (i>0) stay single-peer to avoid flooding.
                            const FRONTIER_REDUNDANCY: usize = 3;
                            for i in 0..(max_inflight as u64) {
                                if inflight >= max_inflight { break; }
                                // v0.57 LANE-L (the real 0 blk/s): request the FRONTIER (i==0) from
                                // the EXACT synced_to, not the floor-aligned `frontier_chunk`. The
                                // floor-aligned request was the frontier-stall root: when the base
                                // sits at a non-CHUNK-aligned height (recent-window snap) OR a peer
                                // serves FEWER than CHUNK blocks per reply (responders cap ~4096
                                // while the client now asks 32768), the floor request keeps re-
                                // fetching the SAME already-stored sub-range and synced_to never
                                // crosses the chunk — got>0 yet +0 advance, frozen. Requesting from
                                // synced_to means every partial fill chains immediately. Look-ahead
                                // (i>0) stays CHUNK-aligned above the frontier; the store dedups any
                                // overlap by height.
                                let start = if i == 0 { store.synced_to() } else { frontier_chunk + i * CHUNK };
                                if start >= peer_best { break; }          // past the tip
                                if !assigned.insert(start) { continue; }  // already in flight
                                let fanout = if i == 0 { FRONTIER_REDUNDANCY.min(healthy.len()).max(1) } else { 1 };
                                for k in 0..fanout {
                                    if inflight >= max_inflight { break; }
                                    let peer = healthy[(rr + k) % healthy.len()];
                                    let payload = serde_json::to_vec(
                                        &BackfillReq { from: start, to: start + CHUNK, headers_only: true, codec: 1 }
                                    ).unwrap();
                                    let n = net.clone();
                                    let tx = done_tx.clone();
                                    let peer_str = peer.to_string();
                                    inflight += 1;
                                    req_n += 1;
                                    tokio::spawn(async move {
                                        let r = tokio::time::timeout(REQ_TIMEOUT, n.send_request(peer, payload)).await;
                                        let bytes = match r { Ok(Ok(b)) => Some(b), _ => None };
                                        let _ = tx.send((start, peer_str, bytes));
                                    });
                                }
                                rr = rr.wrapping_add(fanout);
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
                        } else if recent_only_rt.load(Ordering::Relaxed)
                            && store.best_height() > now_synced && last_advance_t.elapsed() >= Duration::from_secs(2) {
                            // SPINE-BREAK fix: the base-skip is a LIGHT-MONITOR-ONLY heuristic now.
                            // In FULL-ARCHIVE mode (`!recent_only`) advancing `base` past a hole would
                            // SILENTLY ABANDON blocks — exactly the corruption of the "hold every block"
                            // promise that masked the ~499k stall. So in full-archive we do NOT skip:
                            // the frontier request (i==0 from the exact `synced_to`) + the LANE-P
                            // exact-height stall-breaker keep hammering the missing height genesis-up,
                            // and if it's genuinely unfillable the verified-watermark watchdog below
                            // surfaces a LOUD `sync_failure` naming it — never a silent base creep.
                            // v0.16: STABLE-SYNC gate. Only skip genuinely UNSERVABLE LOW history:
                            // advance base when a HIGHER block has actually been RECEIVED
                            // (best_height > frontier) yet the contiguous frontier won’t move — a
                            // real gap from pruned-low ranges. Gating on best_height (the true max
                            // received) instead of the possibly-SEEDED peer_best means that AT the
                            // mesh’s top serving ceiling (best_height == frontier) we do NOT creep
                            // base toward an unreachable/seeded tip — the sync HOLDS at the real
                            // served height instead of thrashing base forever. This was the
                            // instability set_known_tip exposed.
                            // v0.21.1: a monitor whose received tip is far above the contiguous
                            // frontier is sitting on an UNSERVABLE MIDDLE (mesh serves genesis-low +
                            // recent, not the 6M-block middle). Crawling base one chunk per 2s would
                            // take ~an hour for a 600k gap (the "0 blk/s, 560k behind" parked bug).
                            // In monitor mode, JUMP base straight to the recent contiguous window
                            // under best_height so synced snaps to the live head in one step.
                            // (full-sync recent_only=false keeps the genesis-anchored +CHUNK crawl.)
                            let new_base = if recent_only_rt.load(Ordering::Relaxed) && store.best_height() > now_synced + RECENT_WINDOW {
                                store.best_height().saturating_sub(RECENT_WINDOW).max(sync_base)
                            } else {
                                store.base().saturating_add(CHUNK)
                            };
                            crate::tlog!("[sync] gap at frontier {} (received up to {}) — base → {} (jump to recent window)", now_synced, store.best_height(), new_base);
                            store.set_base(new_base);
                            last_synced_seen = store.synced_to();
                            last_advance_t = Instant::now();
                        }
                        let now_synced = store.synced_to();
                        let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                        s.blocks_synced = now_synced;
                        s.base = store.base();
                        s.sync_total = now_synced;
                        s.sync_cursor = now_synced;
                        s.sync_height = now_synced;
                        s.fetched_total = fetched_session;
                        s.peer_count = net.peer_count();
                        if now_synced > s.peer_best_height { s.peer_best_height = now_synced; }
                        // v0.22: a MONITOR displays the VERIFIED LIVE TIP, not the contiguous
                        // backfill frontier. The newest blocks aren't reliably range-served in
                        // real time, so contiguous `synced` ALWAYS lags the head — an unwinnable
                        // race that left the bar stuck "Nk behind / 0 blk/s". A light monitor's
                        // job is to track + verify the live tip (the signed tip-proof in
                        // peer_best), so show THAT as the sync height → the bar reads AT TIP.
                        // The cryptographic spine watermark (⛓✓ = s.verified) stays honest.
                        if recent_only_rt.load(Ordering::Relaxed) && s.peer_best_height > now_synced {
                            s.blocks_synced = s.peer_best_height;
                            s.sync_height = s.peer_best_height;
                            s.sync_total = s.peer_best_height;
                        }
                        // 0.77: the mode is live-flippable — refresh the UI flag every tick
                        // (was set once at launch) so the hero labels follow [F] instantly.
                        s.light_mode = recent_only_rt.load(Ordering::Relaxed);
                        // LANE-P v0.59: surface WHY the frontier is parked (never a silent 0).
                        // Cleared the instant the contiguous frontier advances again.
                        let net_tip = s.peer_best_height;
                        let stalled_for = last_advance_t.elapsed();
                        s.stall_reason = if net_tip > now_synced && stalled_for >= Duration::from_secs(6) {
                            format!("no advance {}s @ {} (gap {}) — retrying exact [{}..] from a rotating peer",
                                stalled_for.as_secs(), now_synced, net_tip.saturating_sub(now_synced), now_synced)
                        } else {
                            String::new()
                        };
                        s.last_message_at = Some(Instant::now());
                    }

                    // ── v0.31 DEEP DEBUG: comprehensive sync snapshot every 2s ──────────────
                    // One dense line with EVERYTHING needed to diagnose a stall from the log/Sync
                    // Log tab: contiguous synced vs the live tip + gap, the displayed-ish rate, how
                    // many backfill requests went out and how they resolved (LEAD/EMPTY/TIMEOUT),
                    // in-flight + assigned, peer counts, bytes pulled, and tip-oracle freshness.
                    if last_dbg.elapsed() >= Duration::from_secs(2) {
                        last_dbg = Instant::now();
                        let (synced_now, peers, hpeers, pbest, tip_age, mesh) = {
                            let s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                            let now = Instant::now();
                            let hp = net.connected_peers().into_iter()
                                .filter(|p| peer_bench.get(&p.to_string()).map_or(true, |&u| now >= u)).count();
                            (store.synced_to(), s.peer_count, hp, s.peer_best_height,
                             s.last_tip_at.map(|t| t.elapsed().as_secs()).unwrap_or(9999), s.mesh_peer_count)
                        };
                        let gap = pbest.saturating_sub(synced_now);
                        let upt = loop_start.elapsed().as_secs().max(1);
                        let win = req_n.max(1);
                        crate::tlog!(
                            "[DBG] up={upt}s synced={synced_now} tip={pbest} gap={gap} | reqs={req_n} lead={lead_n}({:.0}%) empty={empty_n} timeout={timeout_n}({:.0}%) | inflight={inflight} assigned={} fetched={fetched_session} bytes={}MB | peers={peers}(mesh {mesh}, healthy {hpeers}) tip_age={tip_age}s base={}",
                            lead_n as f64 / win as f64 * 100.0,
                            timeout_n as f64 / win as f64 * 100.0,
                            assigned.len(), bytes_session / 1_048_576, store.base()
                        );
                    }

                    // ── SLOW PERIODIC (1.5s): verify the spine + flush the memtable ──────────
                    // verify_to walks only NEW contiguous headers (precheck + parent linkage) and
                    // persists the watermark; a small budget keeps each pass bounded. flush() rolls
                    // the growing memtable to an SST so it can't balloon during a multi-M sync.
                    if last_verify.elapsed() >= Duration::from_millis(1500) {
                        last_verify = Instant::now();
                        // ── LANE-S: GENESIS-ANCHOR CHECK (full-sync ONLY) ──────────────────────
                        // Key the persisted watermarks to the genesis fingerprint so a testnet
                        // restart (fresh genesis) auto-wipes the stale OLD-chain watermarks.
                        //
                        // ⚠️ REGRESSION FIX (v0.70 → v0.71.x): the block at `base` is the genesis
                        // fingerprint ONLY in full-sync mode, where `base` == the true genesis anchor
                        // (height 1) and is STABLE. In recent-window / light mode `base` is a MOVING
                        // checkpoint that snaps FORWARD as the window advances (e.g. 3.04M → 3.12M);
                        // hashing the block there and comparing to the stored anchor false-fired a
                        // reset on EVERY snap — wiping synced 3.08M → 0 and re-syncing from genesis
                        // every few minutes (the "4 peers but 3.1M gap" churn). So only key the
                        // genesis when `base` is the genuine genesis anchor (≤1). In light mode the
                        // oracle-tip-drop heuristic + sane_raise already handle testnet resets.
                        let base_g = store.base();
                        let mut genesis_reset = false;
                        if base_g <= 1 && store.has_height(base_g) {
                            if let Some(hdr) = store.get_header_at_height(base_g) {
                                if store.note_genesis(&hex::encode(hdr.hash())) {
                                    genesis_reset = true;
                                    crate::tlog!("[sync] LANE-S: genesis CHANGED at base {base_g} → wiped stale watermarks, self-healing to the fresh chain");
                                    clear_persisted_tip(); // LANE-S (b): drop the pre-reset cached tip
                                    let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                    s.peer_best_height = 0;
                                    s.verified = 0;
                                    s.blocks_synced = 0;
                                    s.reset_pending = false; // the wipe is done in-line here
                                }
                            }
                        }
                        if !genesis_reset {
                        // v0.15.0 perf: 40k/1.5s capped VERIFIED throughput at ~26.6k blk/s;
                        // 60k/1.5s lifted it to ~40k blk/s against the verify core's then-
                        // measured 52k/s.
                        // v0.33 (1M-blk/s lane): the verify step got ~5× cheaper — linkage now
                        // compares the parent's STORED ingest hash (32-byte memcmp) instead of
                        // re-JSON-hashing the ~1 KB header (~15-25 µs) every step, so a step is
                        // ≈2 db reads + bincode + precheck (~4-6 µs). 240k × ~5 µs ≈ 1.2 s
                        // worst-case loop hold — the SAME wall-clock the old 60k × ~25 µs cost —
                        // while lifting the verified-watermark ceiling to ~160k blk/s. The
                        // budget only binds during catch-up; steady-state verifies arrivals.
                        const VERIFY_BUDGET: u64 = 240_000;
                        // SPINE-BREAK fix: run verify + classify + watchdog under catch_unwind so a
                        // panic in the verify/store/flush path can NEVER poison the state mutex or
                        // kill the sync thread (a dead thread = frozen TUI). On a caught panic we log
                        // loud and continue; the next tick re-runs and the watchdog still surfaces a
                        // real stall. (All lock sites already recover poison via `into_inner`, so
                        // this is belt-and-suspenders for the thread itself.)
                        let verify_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let report = crate::chain_verify::verify_to(&mut store, VERIFY_BUDGET);
                            let class = crate::gap_sync::classify_break(&report);
                            let _ = store.flush();

                            // VERIFIED-watermark watchdog bookkeeping (drives the LOUD no-progress fail).
                            let verified_now = report.verified_to;
                            if verified_now > last_verified_seen {
                                last_verified_seen = verified_now;
                                last_verified_advance_t = Instant::now();
                            }
                            let frontier = store.synced_to();
                            let best = store.best_height();
                            let stalled = last_verified_advance_t.elapsed();

                            let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                            s.verified = verified_now;
                            match class {
                                // Corruption (parent-linkage / precheck / corrupt-hash): surface
                                // immediately + forever — retrying can't fix forged/inconsistent headers.
                                crate::gap_sync::BreakClass::Fatal(h, reason) => {
                                    s.verify_break = Some(format!("h={h}: {reason}"));
                                    s.sync_failure = Some((h, reason.clone()));
                                    if !failure_announced {
                                        failure_announced = true;
                                        crate::tlog!("[sync] ✗ SPINE BREAK (FATAL) at height {h}: {reason} — the downloaded chain does NOT form one connected spine");
                                    }
                                }
                                // No corruption — but is the contiguous frontier WEDGED on an
                                // unfillable hole (a higher block held, frontier parked)? The shared
                                // `gap_sync::watchdog_verdict` decides; it deliberately ignores a merely
                                // CLAIMED higher tip (lying-tip/eclipse) — only a really-RECEIVED higher
                                // block proves a hole, so a quiet caught-up monitor never false-fires.
                                crate::gap_sync::BreakClass::Clean
                                | crate::gap_sync::BreakClass::NeedHeight(_) => {
                                    s.verify_break = None;
                                    match crate::gap_sync::watchdog_verdict(
                                        frontier, best, stalled, Duration::from_secs(watchdog_secs),
                                    ) {
                                        Some(f) => {
                                            if !failure_announced {
                                                failure_announced = true;
                                                crate::tlog!("[sync] ✗ SPINE BREAK (STALL) — {}", f.reason);
                                            }
                                            s.sync_failure = Some((f.height, f.reason));
                                        }
                                        None => {
                                            // advancing, or genuinely caught up to what peers serve →
                                            // clear any prior stall + re-arm the announcer.
                                            s.sync_failure = None;
                                            failure_announced = false;
                                        }
                                    }
                                }
                            }
                        }));
                        if verify_res.is_err() {
                            crate::tlog!("[sync] ⚠ verify/watchdog tick PANICKED — recovered (sync thread alive, mutex un-poisoned); continuing");
                        }
                        } // end if !genesis_reset
                    }

                    // Yield: the request tasks run on worker threads (their results queue in
                    // done_rx regardless), so a short tick keeps the loop from busy-spinning
                    // while staying responsive to gossip + completions.
                    // v0.25.5: CPU throttle. At the head we are TRACKING (not bulk-syncing), so a
                    // slower cadence holds the tip at a fraction of the CPU — the main loop was
                    // pegging a full core re-draining the gossip flood + re-walking the verifier.
                    // v0.27 PROOF-OF-USEFUL-SYNC: at the tip the loop used to just sleep (0% CPU).
                    // Instead, spend that idle CPU re-deriving the stored spine's BLAKE hashes —
                    // the SAME hash methodology as mining, but the work HARDENS sync trust (deeper
                    // verification coverage). Bounded per tick so it is productive, not a core-hog.
                    let at_tip_idle = peer_best > 0 && store.synced_to().saturating_add(CHUNK) >= peer_best;
                    if at_tip_idle {
                        let lo = store.base().max(1);
                        let hi = store.synced_to();
                        if hi > lo {
                            // v0.28: batch-cache the window ONCE (kills the per-header DB-read
                            // bottleneck that capped pos at ~190 blk/s), then re-verify the
                            // in-memory batch with BLAKE every tick — a real useful-hashrate.
                            if pos_window_base != lo || pos_window.is_empty() {
                                pos_window.clear();
                                pos_bytes.clear();
                                let mut h = lo;
                                while h < hi && pos_window.len() < 8192 {
                                    if let Some(hdr) = store.get_header_at_height(h) {
                                        pos_bytes.extend_from_slice(hdr.hash().as_ref()); // derive each header hash ONCE (the serde cost, amortized)
                                        pos_window.push(hdr);
                                    }
                                    h += 1;
                                }
                                pos_window_base = lo;
                            }
                            // v0.29.5 SIMD (flux_optimize_analyze flagged SIMD, ~35%): instead of
                            // re-serializing every header per tick (the ~5.2k blk/s cap), run ONE
                            // AVX2-accelerated blake3 over the whole cached window-digest buffer.
                            // blake3 auto-vectorizes on large inputs -> GB/s. This is the
                            // useful-hashrate + the spine-checkpoint commitment over the window.
                            let ckpt_root = blake3::hash(&pos_bytes);
                            let did = pos_window.len() as u64;
                            pos_acc += did;
                            pos_total_session += did;
                            if pos_t.elapsed() >= Duration::from_secs(1) {
                                let r = pos_acc as f64 / pos_t.elapsed().as_secs_f64().max(1e-6);
                                let mut st = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                st.pos_rate = r;
                                st.pos_total = pos_total_session;
                                pos_acc = 0;
                                pos_t = Instant::now();
                            }
                            // v0.28: gossip a SPINE-CHECKPOINT so OTHER light nodes can trust this
                            // node's re-verified recent window and skip re-verifying it themselves.
                            if ckpt_t.elapsed() >= Duration::from_secs(15) && did > 0 {
                                let ck = format!("{{\"type\":\"spine-checkpoint\",\"net\":\"sigil-g0\",\"from\":{},\"to\":{},\"count\":{},\"root\":\"{}\"}}", lo, lo + did, did, ckpt_root.to_hex());
                                let _ = net.publish("/sigil/g0/spine-checkpoint", ck.into_bytes());
                                crate::tlog!("[pos] gossiped spine-checkpoint [{}..{}] root {}", lo, lo + did, &ckpt_root.to_hex().as_str()[..16]);
                                ckpt_t = Instant::now();
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await; // brief yield between work batches
                    } else {
                        let idle_ms = if peer_best == 0 { 75 } else { 10 };
                        tokio::time::sleep(Duration::from_millis(idle_ms)).await;
                    }
                }
            });
        });

        P2PBlockSync { state, new_blocks, stop_tx: Some(stop_tx), recent_only, rebase_pending }
    }

    /// 0.77: `None` when the sync thread holds the lock RIGHT NOW (heavy ingest/flush) —
    /// the caller keeps rendering its previous clone instead of blocking the draw thread.
    pub fn poll_state(&self) -> Option<P2PSyncState> {
        self.try_state().map(|g| g.clone())
    }

    /// 0.77: try_lock — on contention the blocks simply stay queued for the next frame
    /// (the buffer is capped upstream), never stalling the render loop.
    pub fn drain_new_blocks(&self) -> Vec<StoredBlock> {
        match self.new_blocks.try_lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(std::sync::TryLockError::Poisoned(p)) => std::mem::take(&mut *p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Vec::new(),
        }
    }
}

impl Drop for P2PBlockSync {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use super::zstd_decompress_body;

    /// v0.33 interop gate for the zstd wire: the SERVER compresses with the C-backed
    /// `zstd` crate (zstd::encode_all level 1 — exactly what sigil-node's codec=1 path
    /// calls); the CLIENT decompresses with pure-Rust `ruzstd`. This proves the
    /// cross-implementation roundtrip byte-exactly, plus the malformed-frame and
    /// bomb-guard rejection paths. If ruzstd ever regresses on a standard frame,
    /// this fails the build before a release ships a monitor that can't sync.
    #[test]
    fn zstd_wire_interop_c_encoder_to_rust_decoder() {
        // Body shaped like real wire data: long compressible runs + an incompressible tail.
        let mut body = Vec::with_capacity(220_000);
        for i in 0..50_000u32 {
            body.extend_from_slice(&(i / 7).to_le_bytes());
        }
        body.extend((0..4096u64).map(|i| (i.wrapping_mul(2654435761) % 251) as u8));

        let z = zstd::encode_all(&body[..], 1).expect("C zstd encode (server side)");
        assert!(z.len() < body.len() / 3, "frame actually compressed: {} -> {}", body.len(), z.len());

        let back = zstd_decompress_body(&z).expect("ruzstd decode (client side)");
        assert_eq!(back, body, "byte-exact C-encoder -> Rust-decoder roundtrip");

        assert!(zstd_decompress_body(b"definitely not a zstd frame").is_none(), "garbage rejected");
        assert!(zstd_decompress_body(&[]).is_none(), "empty rejected");
    }
}

#[cfg(test)]
mod sync_math_tests {
    //! Pure sync arithmetic + wire-tip extraction (Tier 3). A bug here makes the
    //! fast-snap probe target the wrong height or fail to learn a peer's tip.
    use super::{align_base, max_header_height, BackfillResp};

    #[test]
    fn align_base_snaps_below_h_and_clamps_to_floor() {
        // chunk-aligned at/below h.
        assert_eq!(align_base(10_000, 4_096, 0), 8_192, "2*4096");
        assert_eq!(align_base(8_192, 4_096, 0), 8_192, "exact boundary stays");
        assert_eq!(align_base(100, 4_096, 0), 0, "below one chunk floors to 0");
        // sync_base floor wins when the alignment would go below it.
        assert_eq!(align_base(100, 4_096, 500), 500, "clamped up to the servable floor");
    }

    #[test]
    fn align_base_invariants_hold_over_a_sweep() {
        for &chunk in &[1u64, 2, 1_024, 4_096] {
            for h in [0u64, 1, 5_000, 1_000_000, u64::MAX / 2] {
                for &base in &[0u64, 4_096, 10_000] {
                    let a = align_base(h, chunk, base);
                    assert!(a >= base, "never below the servable floor");
                    // When the floor isn't binding, the result is chunk-aligned and ≤ h.
                    if a > base {
                        assert_eq!(a % chunk, 0, "must be chunk-aligned");
                        assert!(a <= h, "must not jump past the requested height");
                    }
                }
            }
        }
    }

    #[test]
    fn max_header_height_reads_legacy_json_tip() {
        // The legacy full-block JSON codec: max over the headers' heights.
        let resp = BackfillResp {
            blocks: vec![
                serde_json::json!({"header": {"height": 12}}),
                serde_json::json!({"header": {"height": 4_096_777}}),
                serde_json::json!({"header": {"height": 5}}),
            ],
        };
        let bytes = serde_json::to_vec(&resp).unwrap();
        assert_eq!(max_header_height(&bytes), Some(4_096_777));
    }

    #[test]
    fn max_header_height_is_none_on_empty_or_garbage() {
        let empty = serde_json::to_vec(&BackfillResp { blocks: vec![] }).unwrap();
        assert_eq!(max_header_height(&empty), None, "no headers → no tip");
        assert_eq!(max_header_height(b"not a real wire payload"), None);
        assert_eq!(max_header_height(b""), None);
    }
}
