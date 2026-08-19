// sigil-top/src/block_sync.rs — Real P2P block sync via flux-p2p mesh (v0.7.1)
//
// v0.7.1: Uses flux_p2p::NetworkManager::for_sigil() builder and the new
// event-driven subscribe() API. No more polling — the notifier wakes us
// when blocks arrive on /sigil/g0/blocks.

use serde::{Deserialize, Serialize};
use super::block_store::BlockStore;
use sigil_header::SigilBlockHeaderV0;
use flux_turbo_sync::continuity::BandwidthContinuity;

use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use std::collections::HashMap;

// ── v3 sync sprint (2026-06-19): block_sync split into lane-owned modules so the
// three sync lanes never collide on one file. mod.rs owns the launch() orchestrator,
// the wire structs, and shared infra (HTTP clients, sane_raise/SANE_LEAD). Each
// submodule is one lane's surface; the glob `use`s keep launch()'s calls unqualified.
mod fetch;   // LANE-A net/transport  (rocky-sync-A)
mod verify;  // LANE-B decode+verify  (rocky-sync-B)
mod commit;  // LANE-C storage/commit (rocky-sync-C)
mod skeleton_store; // flat append-only skeleton prefix store (the 10M-blk/s path to 100k)
mod skel_flux; // ADOPT the native flux-db skeleton extension (flux_db::skeleton)
mod archive; // PASS 2: background full-archive body backfill (trustless vs skeleton hashes)
mod fast_forward; // V7-INGEST: PASS-2 body sink routed through LANE-1 SST-ingest (commit→DB fast-forward)
pub mod ledger; // ONE-CHAIN P2: persistent verified sync of the LEDGER header chain (rocky)
#[allow(unused_imports)]
pub(crate) use skeleton_store::SkeletonStore;
use fetch::*;
use verify::*;
use commit::*;

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


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackfillResp {
    pub blocks: Vec<serde_json::Value>,
}


#[derive(Debug, Clone, Default)]
pub struct P2PSyncState {
    pub running: bool,
    pub peer_count: u32,
    pub mesh_peer_count: u32,
    pub peer_best_height: u64,
    /// v7.1.19 fix: has `peer_best_height` EVER been confirmed by a live oracle
    /// answer this session? A cold-start seed from the on-disk `last-tip` cache
    /// (`read_persisted_tip`, offline-resilience) sets `peer_best_height`
    /// directly with ZERO validation — it's an unverified guess, not a
    /// live-confirmed value. The reset-detection loop below normally requires
    /// 3 CONSECUTIVE oracle polls to disagree with `peer_best_height` before
    /// correcting it (deliberately conservative, to avoid one flaky reading
    /// overriding a value the oracle itself vouched for moments ago). That
    /// conservatism is wrong for a disk-cache seed, which has never been
    /// vouched for by anything — it should be corrected on the FIRST oracle
    /// answer, not the third. Root-caused live 2026-08-15: a stale pre-
    /// genesis-reset cache (tens of millions on a chain now ~350K tall) froze
    /// `peer_best_height` there for 90+ minutes because 3-in-a-row never
    /// coincided with whatever made the operator's session unstable enough
    /// to lose consecutive-poll continuity.
    pub peer_best_oracle_confirmed: bool,
    pub blocks_synced: u64,
    pub last_message_at: Option<Instant>,
    /// v0.26: when the tip-poller last got a fresh tip. UI shows STALE if this ages out
    /// (oracle down / network partition) instead of a falsely confident "AT TIP".
    pub last_tip_at: Option<Instant>,
    pub connected_delta: bool,
    pub connected_epsilon: bool,
    /// v7.1.13: count of failed outbound dials/handshakes this session. Non-zero
    /// with peer_count==0 means "we ARE trying — the dials are being rejected"
    /// (previously indistinguishable from not dialing at all).
    pub dial_failures: u64,
    /// v0.7.0: Latest sync progress from the P2P mesh (consumed by TUI gauge).
    pub sync_height: u64,
    pub sync_hash_hex: String,
    pub sync_total: u64,
    /// v0.7.6: blocks pulled via content-addressed backfill (flux-sync), verified.
    pub backfilled: u64,
    /// v6.0.0 LANE-C turbo-commit: live durable-commit rate (blk/s) + bulk-load armed flag,
    /// surfaced in the SYNC hero so the operator SEES the write-back ring working.
    pub commit_rate: f64,
    pub turbo_armed: bool,
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
    // Turbo Sync X + invented Continuity for continuous high download bandwidth network
    pub turbo_continuity: BandwidthContinuity,
}

/// v7.0.21: the background store-opener's payload — the opened store plus an optional
/// operator-facing note (e.g. "primary store unavailable; using temp store").
pub struct OpenedStore {
    pub store: BlockStore,
    pub note: Option<String>,
}

/// How `launch_src` receives its BlockStore: already opened (all legacy call sites),
/// or arriving later from a background opener thread (dial-while-opening).
enum StoreSource {
    Ready(Box<BlockStore>),
    Deferred(std::sync::mpsc::Receiver<OpenedStore>),
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

/// LANE-B fold fast-path dependency: the live DNS SQIsign anchor tip as (height, block_hash).
/// STUB (returns None) — lands the fast-forward call site with ZERO regression (skipped until this
/// returns a value). The real impl (NEXT) must, per B's #417 consumer contract: fetch the DNS TXT
/// (`fetch_dns_anchor` + `sigil_dns_anchor::decode`), VERIFY the SQIsign sig over
/// (block_hash‖4 roots‖height‖epoch), and REJECT stale anchors (monotonic epoch, age ≤ MAX) before
/// returning Some — `fast_forward_to_anchored_checkpoint` trusts whatever this hands it.
async fn dns_anchor_tip() -> Option<(u64, sigil_header::BlockHash)> {
    // DEV-ONLY bench source (interim, until A lands the real DNS+SQIsign-verify fetch): the live
    // `_sigil-tip` DNS TXT is currently a dead template (A #449), so SIGIL_SNAPSHOT_ANCHOR=<height>:<hex32>
    // lets us exercise the full snapshot-pull → fast_forward path + measure blk/s NOW. Unset → None
    // (no-op, zero regression). NEVER trusts a network anchor without B's verify_signed_anchor (#417).
    if let Ok(s) = std::env::var("SIGIL_SNAPSHOT_ANCHOR") {
        if let Some((h, hx)) = s.split_once(':') {
            if let (Ok(height), Ok(bytes)) = (h.parse::<u64>(), hex::decode(hx)) {
                if let Ok(hash) = <[u8; 32]>::try_from(bytes.as_slice()) {
                    return Some((height, hash));
                }
            }
        }
    }
    // REAL anchor path (LANE-A, landed 4ba6884): fetch the producer-signed `v=sigil1`
    // anchor → sigil_dns_anchor::verify_signed_anchor (SQIsign) + key_id + freshness +
    // monotonic epoch. None until a real signed anchor is published (operator pins
    // SIGIL_ANCHOR_PK_HEX) → zero regression; the dev-inject above covers the bench.
    //
    // v7.1.39 (grogu-sync-perf, 2026-08-19) — THE root cause of "0 peers forever" /
    // snapshot-pull never firing, root-caused live via a full swarm-event trace: this
    // function used to call `fetch::fetch_verified_anchor_tip()` (a SYNCHRONOUS reqwest
    // ::blocking HTTP call) directly, inline, from THIS async fn while it was still a
    // plain `fn` invoked straight from the block_sync tokio task. A blocking reqwest
    // client builds+tears down its own internal mini tokio runtime per call; doing that
    // from a thread that is ALREADY a tokio worker panics ("Cannot drop a runtime in a
    // context where blocking is not allowed"), and since this call runs the instant the
    // FIRST peer connects (the LANE-A gate below fires on !peers.is_empty()), the whole
    // p2p sync task died silently on `tokio::spawn`'s un-awaited JoinHandle — visible only
    // via `boot_trace()` (main.rs, /tmp/sigil-top-startup.log), NEVER on stdout/stderr,
    // which is why it went undiagnosed all session despite extensive tlog! diagnostics
    // (those were ALSO silently dropped in headless mode until the separate IN_TUI fix
    // above — two independently-invisible bugs stacked on the exact same code path).
    // Fix: run the blocking HTTP fetch on tokio's dedicated blocking-pool thread via
    // spawn_blocking (the standard, sanctioned way to call blocking code from async Rust
    // — the same pattern the tip-poller's `fetch_live_tip_blocking()` already uses via a
    // plain `thread::spawn`, just via tokio's pool instead of a raw OS thread since this
    // caller is already inside the runtime). No behavior change to the fetch itself.
    tokio::task::spawn_blocking(fetch::fetch_verified_anchor_tip).await.ok().flatten()
}

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
    pub fn launch(store: BlockStore, recent_only: bool) -> Self {
        Self::launch_src(StoreSource::Ready(Box::new(store)), recent_only)
    }

    /// v7.0.21 DIAL-WHILE-OPENING: launch the engine with a store that ARRIVES LATER.
    /// The mesh (net start + eager bootstrap dials) comes up immediately; the engine
    /// then blocks on `rx` for the opener thread's store. A grown store compacting at
    /// open (measured: minutes) no longer holds the whole mesh at 0 peers — dials,
    /// handshakes and gossip warm up during the open.
    pub fn launch_deferred(rx: std::sync::mpsc::Receiver<OpenedStore>, recent_only: bool) -> Self {
        Self::launch_src(StoreSource::Deferred(rx), recent_only)
    }

    fn launch_src(source: StoreSource, recent_only: bool) -> Self {
        // ONE-CHAIN P2: mirror the LEDGER header chain alongside whatever this
        // engine syncs — additive background thread, own store, never blocks.
        ledger::ensure_running();
        // SIGIL_SNAP=1 forces fast-snap even in full-sync (validation / "just track the tip").
        let recent_only_init = recent_only || std::env::var("SIGIL_SNAP").is_ok();
        // 0.77 GENESIS ARCHIVE: the mode is LIVE-FLIPPABLE ([F] reaches a running engine
        // through this atomic — before 0.77 the bool was captured by value and the toggle
        // was a TUI-local no-op). Every base-snap gate below loads it fresh.
        let recent_only = Arc::new(AtomicBool::new(recent_only_init));
        let rebase_pending = Arc::new(AtomicBool::new(false));
        let recent_only_rt = recent_only.clone();
        let rebase_pending_rt = rebase_pending.clone();
        let state = Arc::new(Mutex::new(P2PSyncState {
            turbo_continuity: BandwidthContinuity::default(),
            ..P2PSyncState::default()
        }));
        // v2.0.0: the sync thread (spawned below) moves `state` in; keep a shared
        // Arc clone for the returned struct so both observe the same Mutex.
        let state_struct = state.clone();
        let new_blocks = Arc::new(Mutex::new(Vec::new()));
        let (stop_tx, stop_rx) = mpsc::channel();

        let state_clone = state.clone();
        let new_blocks_clone = new_blocks.clone();

        // v0.21: DEDICATED tip-poller thread. The monitor's fast-snap needs a FRESH live tip
        // in peer_best; the in-runtime async fetch was non-deterministically starved by the
        // backfill/verify workload (peer_best froze → the monitor parked behind the tip at
        // 0 blk/s). A standalone OS thread with BLOCKING reqwest polls every 3s, immune to
        // that contention, and seeds peer_best directly.
        // 0.77: the dedicated tip-poller spawn gated on the LAUNCH-TIME mode (a
        // full-archive launch relied on the in-loop async fetch — called "acceptable").
        // v7.1.13: it was NOT acceptable — measured in the 2026-08-09 Windows stall, a
        // FULL-ARCHIVE client's in-loop fetch starved and the displayed tip froze at a
        // weeks-old value (33,598,726) while the real tip was 220k higher. The dedicated
        // poller now runs in EVERY mode; it only ever raises peer_best.
        if true {
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
                                // v7.1.19 fix: a `peer_best_height` that's still just the raw
                                // disk-cache cold-start seed (never oracle-confirmed) gets NO
                                // benefit of the doubt — adopt the FIRST oracle answer outright,
                                // instead of requiring the same 3-consecutive-poll streak a
                                // previously-verified value would deserve. See the field's doc
                                // comment on `P2PSyncState::peer_best_oracle_confirmed`.
                                if !s.peer_best_oracle_confirmed && pb != h {
                                    crate::tlog!(
                                        "[tipfetch] first oracle confirmation this session — adopting {} over unverified seed {}",
                                        h, pb
                                    );
                                    s.peer_best_height = h;
                                    s.peer_best_oracle_confirmed = true;
                                    s.blocks_synced = s.blocks_synced.min(h);
                                    s.sync_height   = s.sync_height.min(h);
                                    s.sync_total    = s.sync_total.min(h);
                                    s.verified      = s.verified.min(h);
                                    if s.base > h { s.base = h; }
                                    if pb > h { s.reset_pending = true; } // was ABOVE truth — wipe the poisoned store
                                    reset_streak = 0;
                                    clear_persisted_tip();
                                    persist = Some(h);
                                } else if pb > 0 && h < pb / 2 && pb - h > 100_000 {
                                    reset_streak += 1;
                                    // 0.95: never wipe authoritative local watermarks on a single
                                    // oracle read. Even a 4x drop must repeat for three polls; the
                                    // sync loop also waits for at least one mesh peer before applying
                                    // the destructive store reset.
                                    if reset_streak >= 3 {
                                        s.peer_best_height = h; // RESET to the live oracle tip
                                        s.peer_best_oracle_confirmed = true;
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
                                    s.oracle_tip = h; // the signed oracle anchor — gates gossip raises
                                    if h > s.peer_best_height {
                                        reset_streak = 0;
                                        s.peer_best_height = h;
                                        s.peer_best_oracle_confirmed = true;
                                        persist = Some(h);
                                    } else if s.peer_best_height > h.saturating_add(SANE_LEAD) {
                                        // v0.59 ORACLE-AUTHORITATIVE: peer_best drifted ABOVE the signed
                                        // oracle by more than a sane lead — a phantom gossip claim (the
                                        // 1.4M post-genesis-reset ghost) that the drastic-drop branch
                                        // above MISSES (it isn't < pb/2). The signed oracle wins: snap
                                        // peer_best back to it + clamp the progress watermarks so the UI
                                        // can't chase a tip that doesn't exist, and clear the persisted
                                        // seed so a restart doesn't re-poison from disk.
                                        reset_streak += 1;
                                        if reset_streak >= 3 {
                                            s.peer_best_height = h;
                                            s.peer_best_oracle_confirmed = true;
                                            s.blocks_synced = s.blocks_synced.min(h);
                                            s.sync_height   = s.sync_height.min(h);
                                            s.sync_total    = s.sync_total.min(h);
                                            s.verified      = s.verified.min(h);
                                            if s.base > h { s.base = h; }
                                            s.reset_pending = true; // LANE-S: wipe the store too
                                            reset_streak = 0;
                                            clear_persisted_tip();
                                            persist = Some(h);
                                        }
                                    } else {
                                        reset_streak = 0;
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
            //
            // v7.1.39 (grogu-sync-perf, 2026-08-19): investigated a stall found live tonight —
            // sync freezes for extended periods, `inflight` requests never resolve despite a
            // 10s REQ_TIMEOUT, no panic. Initially suspected worker-thread starvation (several
            // sections do genuinely synchronous CPU-bound work inline — rayon `.par_iter()`
            // decode, blake3 hashing, the durable commit flush — none `spawn_blocking`'d, and
            // with only 3 workers a couple of those back-to-back could starve everything else,
            // the same failure class the v0.10.0 comment above already describes). Tested
            // directly by bumping to 16 workers live: **no change** — ruled out. Sequential
            // checkpoint instrumentation (temp, reverted) then caught the real cause in the
            // act: a single rayon-parallel decode of just 5 pages took 9,506 ms — vs the same
            // function's own measured baseline earlier this session (hundreds of thousands of
            // headers/sec). That is box-wide CPU starvation (load ~19, swap 100% full from
            // several concurrent sessions + an auto-triggered build), not a code bug — see
            // memory project_sigil_sync_index_vs_body_gap for the full trail. Left this knob in
            // anyway: it's a safe, env-gated, zero-risk-when-unset tunable in the codebase's
            // established pattern (SIGIL_SYNC_INFLIGHT, SIGIL_COMMIT_BATCH, …), and more workers
            // is still a reasonable lever for genuine (non-external) contention even though it
            // didn't move tonight's specific symptom. Default unchanged (3).
            let worker_threads: usize = std::env::var("SIGIL_SYNC_WORKER_THREADS")
                .ok().and_then(|v| v.parse().ok()).map(|n: usize| n.clamp(2, 32)).unwrap_or(3);
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads).enable_all().build()
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

                // v7.0.21 DIAL-WHILE-OPENING: the mesh is up and the eager bootstrap dials
                // are in flight — NOW wait for the store if it's still opening (a grown
                // store can compact for minutes at open). Handshakes/gossip warm up in the
                // background tokio workers while this future blocks; by the time a big
                // store opens, peers are already connected instead of starting from zero.
                let mut store = match source {
                    StoreSource::Ready(s) => *s,
                    StoreSource::Deferred(rx) => {
                        {
                            let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                            s.running = true;
                            s.stall_reason = "opening local block store (a grown store may compact for minutes) — mesh already dialing".into();
                        }
                        match tokio::task::spawn_blocking(move || rx.recv()).await {
                            Ok(Ok(opened)) => {
                                let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                s.stall_reason = opened.note.clone().unwrap_or_default();
                                crate::tlog!("[sync] store opened (deferred) best={} synced={} — engine proceeding",
                                    opened.store.best_height(), opened.store.synced_to());
                                opened.store
                            }
                            _ => {
                                crate::tlog!("[sync] ✗ store opener failed/disconnected — sync engine cannot start");
                                let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                s.running = false;
                                s.stall_reason = "✗ block store unavailable — sync disabled (see ~/.sigil-top.log)".into();
                                return;
                            }
                        }
                    }
                };

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
                // v7.0.17 REVERTED the v7.0.15 rebase-on-launch: set_base() only (the pre-v7.0.15,
                // known-good behavior). rebase()'s advance_synced walks has_height (a DB get PER
                // height) from base on the boot path — on an existing multi-million-block store that
                // is millions of disk gets, which stalled boot and blanked the screen (the v7.0.16
                // "bricked node" incident). Snapped-watermark self-heal will be redone off the boot
                // path (background/indexed), never with an O(N) walk before first frame.
                store.set_base(sync_base);
                let resume_h = store.synced_to();
                {
                    let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                    s.running = true;
                    s.blocks_synced = resume_h;
                    s.verified = store.verified_to(); // v0.9.0: resume the verified watermark too
                    s.light_mode = recent_only_rt.load(Ordering::Relaxed); // v0.57 (LANE-M): drives honest verified-vs-stored UI
                    // v0.96 SUB-1s COLD START: seed peer_best from the on-disk tip cache in ALL
                    // modes. The v0.32.5 eager seed (+ dedicated tip-poller) was gated behind
                    // `recent_only_init`, so a FULL-ARCHIVE launch started with peer_best=0 — and
                    // the frontier refill's `start >= peer_best` gate held OFF until the first
                    // single-peer probe / CDN tip landed. That gap IS the "0 blk/s at start". One
                    // microsecond disk read makes peer_best>0 from t=0, so the full-archive
                    // frontier (which already fans out to ALL healthy peers) fires on the very
                    // first iteration a peer is connected. Only ever RAISES; the live tip fetch +
                    // probe correct it upward, and chain-reset detection drops a stale cache.
                    if let Some(h) = read_persisted_tip() {
                        if h > s.peer_best_height { s.peer_best_height = h; }
                    }
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
                    .map(|n| n.clamp(1024, 65_536)).unwrap_or(10_000); // v6.5: 10k blocks/sync default (frontier-exact fix v0.57 makes large chunks safe)
                // v0.39: was const 12 — at first boot (empty DB) all slots fire decode
                // bursts at once, pre-TUI, which pressured small/busy machines hard. 8 by
                // default; SIGIL_SYNC_INFLIGHT=1..16 to tune (raise on a beefy box).
                //
                // 🚫 v7.0.6 REGRESSION REVERT (2026-07-20): v7.0.3 raised this to a HARD FLOOR
                // of 16 for a ~2.8x speedup. That FLOOR re-triggered the v0.10.0 frontier-stall
                // bug: with a single serving peer, ~16 slots claim look-ahead chunks up to
                // synced_to+16*CHUNK and commit them AHEAD of the frontier, while the lead chunk
                // (i==0 at exact synced_to) gets skipped once it's `assigned`-claimed-but-not-
                // advancing — so synced_to PARKS, best>frontier, and the verified-watermark
                // watchdog declares "SPINE BREAK — STUCK" (operator saw it wedge at h≈393,265).
                // v7.0.2 synced all 30M to 100% in ~3h because the boost could bottom at 2,
                // keeping the window small enough that the frontier was always fed. Restoring
                // the v7.0.2 behavior EXACTLY. DO NOT raise the default floor again without
                // first fixing frontier-chunk starvation in the refill loop (re-request the
                // lead when synced_to hasn't advanced). Power users can still opt in via the env.
                let max_inflight: usize = std::env::var("SIGIL_SYNC_INFLIGHT").ok()
                    .and_then(|v| v.parse::<usize>().ok()).map(|n| n.clamp(1, 16)).unwrap_or(8);
                // Turbo X continuity: boost inflight when high continuous BW (score and pid_rate) to sustain high download rate
                let max_inflight = {
                    let s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                    let score = s.turbo_continuity.continuity_score;
                    let pid_r = s.turbo_continuity.pid.get_rate().max(5.0);
                    let rate_boost = (pid_r / 50.0).max(0.5).min(2.0);
                    ((max_inflight as f64) * (0.5 + score * 1.5) * rate_boost).max(2.0).min(64.0) as usize
                };
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
                // v0.95 FRONTIER-WEDGE watchdog: count backfill chunks RECEIVED for the
                // frontier range that did NOT advance `synced_to`. A genuine wedge (a forked
                // chunk the store rejects, or an out-of-order squatter blocking the spine
                // block at the frontier) keeps serving real bytes that never splice — but
                // `best` stays == `frontier`, so the verified-watermark watchdog (which needs
                // best>frontier to prove a hole) never fires and the run falls to the generic
                // timeout. This counter IS the wedge evidence: it accrues only when frontier
                // data actually arrives without progress, so it can't false-fire on a quiet
                // caught-up monitor or a merely-claimed (lying) higher tip. Reset on advance.
                let mut frontier_serves_since_advance: u32 = 0;
                // v7.0.18 FRONTIER SELF-HEAL: bounded rollback-and-refetch attempts when honest
                // frontier headers repeatedly refuse to splice (poisoned local seam). Reset on
                // real advance; after MAX attempts the loud SPINE BREAK verdict stands.
                let mut heal_attempts: u32 = 0;
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
                let mut last_rate_time = Instant::now();
                let mut last_rate_bytes: u64 = 0;
                let mut last_dbg = crate::instant_ago(5);
                let loop_start = Instant::now();
                // v0.27 proof-of-useful-sync local accumulators
                let mut pos_cursor: u64 = 0;
                let mut pos_acc: u64 = 0;
                // LANE-C STAGE-B: write-back commit ring — the durable-batch fast commit path.
                // Replaces the per-chunk put_blocks_batch whose flux-db compaction storm is the
                // >20k wall (commit.rs SEAM). arm() defers compaction in a deep gap; flush() is the
                // write-through point before the cursor re-reads synced_to(); finish() folds once.
                let mut commit_ring = commit::CommitBuffer::from_env();
                let mut bulk_armed = false;
                let mut last_committed: u64 = 0;
                let mut last_commit_t = Instant::now();
                let mut pos_total_session: u64 = 0;
                let mut pos_t = Instant::now();
                // v0.28 batched useful-sync: cache the window once + gossip a checkpoint
                let mut pos_window: Vec<sigil_header::SigilBlockHeaderV0> = Vec::new();
                let mut pos_window_base: u64 = 0;
                let mut ckpt_t = Instant::now();
                let mut pos_bytes: Vec<u8> = Vec::new(); // v0.29.5 cached window-digest buffer for SIMD blake3
                // v7.1.32 (grogu-sync-perf): last time the idle useful-hashrate scan came up
                // completely empty (found 0 real headers in [lo, hi]). Backs off re-scanning a
                // known-sparse range every single tick — see the MISS_CAP comment below.
                let mut pos_scan_empty_since: Option<Instant> = None;
                let mut last_probe = crate::instant_ago(10); // pull-height probe timer
                // v0.15.2: far-behind monitor snaps to a recent window once peer_best is known.
                const RECENT_WINDOW: u64 = 2_048;  // v0.21: pin the base just 1 chunk under the live tip
                let mut snapped = false;
                let mut snapshot_attempted = false; // LANE-A snapshot-pull one-shot guard

                // PASS-2 (archive.rs) - persistent skeleton handle shared with the one-shot
                // snapshot-pull commit-hook below, plus the deep-gap body-backfill reply channel.
                // Dormant until pass-1 populates the skeleton (gated on dns_anchor_tip()); then it
                // converges THIS node to a full archive in the background (#156), trustlessly: a
                // fetched body is stored only if it hashes to the skeleton's committed block_hash.
                let skel_dir = format!("{}-skeleton", std::env::var("SIGIL_TOP_DB").unwrap_or_else(|_| "sigil-top-blocks.db".to_string()));
                let mut skel: Option<skel_flux::FluxSkeletonStore> = skel_flux::FluxSkeletonStore::open(&skel_dir, 0).ok();
                let (pass2_tx, pass2_rx) = mpsc::channel::<(u64, Option<Vec<u8>>)>();
                let mut pass2_inflight = false;
                let mut last_pass2 = crate::instant_ago(10);
                let pass2_env = std::env::var("SIGIL_PASS2").map(|v| v != "0").unwrap_or(true);
                // V7-INGEST: when SIGIL_DB_SST_INGEST is on, PASS-2 verified bodies commit via the
                // LANE-1 SST-ingest fast path (~230k blk/s) instead of the ~4k batch_put WAL wall.
                // None (flag off) → byte-identical legacy archive::ingest_bodies_verified path.
                let mut pass2_sink: Option<fast_forward::Pass2Sink> =
                    fast_forward::sst_ingest_active().then(fast_forward::Pass2Sink::from_env);
                // v7.1.35 (grogu-sync-perf): VCATCH — a skeleton/anchor-INDEPENDENT fallback body
                // backfill. Root cause (swarm msg #85-87, saved to memory
                // project_sigil_sync_index_vs_body_gap_2026_08_18): `synced_to` advances via
                // `has_height()` (block_store.rs) which checks ONLY the height->hash index, not the
                // full body — so a LANE-S trust-jump (or any bulk index write) races `synced_to`
                // forward while `verified_to` (which needs the real body via `get_stored_at_height`)
                // stays stuck at genesis. PASS2 is the intended fix but is dormant until pass-1
                // populates the skeleton, which is gated on `dns_anchor_tip()` — confirmed live
                // (2026-08-18) that no signed anchor is published anywhere, including production, so
                // pass2 has never actually run. VCATCH does the same job pass2 would (fetch the
                // missing body range) WITHOUT any skeleton/anchor dependency: it just requests
                // [verified_to, verified_to+CHUNK) directly. `verify_to_parallel` (chain_verify.rs)
                // does its OWN full cryptographic verification once the data lands via the normal
                // commit path, so this needs no skeleton-hash cross-check — it only has to get real
                // bytes onto disk where the existing verifier can find them. Single in-flight,
                // low-priority (does not compete with the frontier refill above), full-archive only.
                let (vcatch_tx, vcatch_rx) = mpsc::channel::<(u64, Option<Vec<u8>>)>();
                let mut vcatch_inflight = false;
                let mut last_vcatch = crate::instant_ago(10);
                let vcatch_env = std::env::var("SIGIL_VCATCH").map(|v| v != "0").unwrap_or(true);
                // v0.96 (fixes a live infinite-retry stall, 2026-08-18): pass2 fetches MATURE
                // headers — each carries a full StarkProof + Wesolowski VDF proof + 2 SQIsign
                // sigs, ~8 KB/header per the zstd_decompress_body comment — not the small/stub
                // genesis-area headers the general sync CHUNK (up to 65_536 via SIGIL_SYNC_CHUNK)
                // was tuned against. A pass2 request at the general CHUNK width can decompress to
                // well past the 64 MiB decode cap; zstd_decompress_body then silently returns
                // None, decode_verify_backfill returns an empty Vec, nothing gets stored, and
                // next_body_gap hands back the EXACT SAME [from,to] on the next 500ms tick —
                // forever. Observed live: peer 12D3KooWB8QjKPjHaZLk5Aj4sn87oXSQmivMmsCNXQxpgEyRA1si
                // re-requested the identical [1123319..=1156087] every ~1-2s for 15+ minutes.
                // Start conservative and AIMD-shrink on decode failure so pass2 always converges
                // to a chunk width that decodes, regardless of how large a header ever grows;
                // grow back toward the CHUNK ceiling once healthy so throughput isn't sacrificed.
                let mut pass2_chunk: u64 = CHUNK.min(2048);

                loop {
                    let tick_t0 = Instant::now();
                    if stop_rx.try_recv().is_ok() {
                        let _ = net.stop().await;
                        break;
                    }

                    // LANE-A snapshot-pull (one-shot, gated). No-op until dns_anchor_tip() is real
                    // (SQIsign-verified + fresh). Bulk-pulls the verified skeleton prefix in codec=2
                    // pages; on success commits skeletons (put_block_raw) + hands off via
                    // fast_forward_to_anchored_checkpoint, then the frontier refill resumes from the
                    // anchor. Err / no codec=2 server → the codec=1 crawl below covers it unchanged
                    // (zero regression). SIGIL_SNAPSHOT=0 disables it.
                    //
                    // v7.1.37 fix: dns_anchor_tip() burns the anchor's epoch as "used" (its internal
                    // monotonic anti-replay guard) the instant it verifies successfully, REGARDLESS of
                    // whether this gate goes on to act on it. A static, once-published anchor's epoch
                    // never increases, so if the FIRST successful verify happened before any peer had
                    // connected (peers.is_empty() still true on a fresh node's early ticks), the anchor
                    // was permanently spent on a no-op — every later tick saw the SAME epoch rejected as
                    // a replay of itself, and snapshot-pull could never fire for the rest of the
                    // process's life. Fix: check the cheap peers/sync_base/env gate FIRST and only call
                    // dns_anchor_tip() (which may consume the one-shot epoch) once it can actually be
                    // acted on this tick.
                    let snap_on = std::env::var("SIGIL_SNAPSHOT").map(|v| v != "0").unwrap_or(true);
                    let peers = net.connected_peers();
                    if !snapshot_attempted && snap_on && store.synced_to() <= sync_base && !peers.is_empty() {
                        if let Some((va_h, va_hash)) = dns_anchor_tip().await {
                            snapshot_attempted = true;
                            let net_c = net.clone();
                            let send = move |peer, payload: Vec<u8>| {
                                let n = net_c.clone();
                                async move { n.send_request(peer, payload).await }
                            };
                            // A's commit-hook flip: the verified skeleton prefix lands in the
                            // flat append-only SkeletonStore (no per-key flux-db commit). base=0:
                            // the producer serves a genesis-anchored prefix (header.base_height=0).
                            // skel handle hoisted to outer loop scope (shared with PASS 2); reused here.
                            // LANE 3 raw-commit seam: pull_snapshot hands the RAW 72B page body
                            // straight to append_raw — no per-page Vec<SkelRec> clone, no re-encode
                            // (SkelRec::encode == wire 72B, proven sigil-header/tests/byte_identity.rs).
                            match fetch::pull_snapshot(&peers, send, |raw: &[u8]| { if let Some(s) = skel.as_mut() { let _ = s.append_raw(raw); } }).await {
                                Ok(v) => match verify::fast_forward_from_authenticated_snapshot(
                                    &mut store, v.anchor_height, &v.anchor_hash, va_h, &va_hash, verify::DEFAULT_FRONTIER_WINDOW,
                                ) {
                                    Ok(vt) => crate::tlog!("[sync] snapshot-pull OK + ANCHOR-AUTHENTICATED: {} recs, anchor h={} == trust root -> verified_to={} (bodies via PASS-2)", v.records, v.anchor_height, vt),
                                    Err(e) => crate::tlog!("[sync] snapshot-pull anchor auth refused ({e}) - crawl covers it"),
                                },
                                Err(e) => crate::tlog!("[sync] snapshot-pull failed ({e:?}) — codec=1 crawl covers it"),
                            }
                        }
                    }

                    // LANE-S CHAIN-RESET SELF-HEAL: the tip-poller flagged a reset (the live tip
                    // is drastically below our peer_best — a fresh genesis). Wipe the block-store
                    // watermarks (synced_to/verified_to/best) so the stale OLD chain is forgotten
                    // and re-downloaded from the fresh tip, and reset the local cursors so the
                    // refill restarts cleanly. NO manual local wipe needed.
                    {
                        let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                        if s.reset_pending {
                            if net.peer_count() == 0 {
                                s.stall_reason = "chain reset pending, but peers=0 — waiting for mesh corroboration before wiping local watermarks".into();
                            } else {
                                s.reset_pending = false;
                                drop(s);
                                store.reset_watermarks();
                                assigned.clear();
                                last_synced_seen = 0;
                                last_advance_t = Instant::now();
                                snapped = false;
                                crate::tlog!("[sync] CHAIN-RESET self-heal — block store watermarks wiped after repeated oracle reset + live mesh peer");
                            }
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
                        // v0.34: transparently inflate a `'Z'`-tagged zstd gossip frame
                        // (legacy `{…}` JSON passes through zero-copy). Lets the light node
                        // ingest compressed live gossip — ~14× less inbound wire once a
                        // producer flips compression on, with no flag-day (mixed fleet ok).
                        let inflated = match inflate_gossip_frame(&data) {
                            Some(b) => b,
                            None => continue, // malformed Z frame → drop like any bad gossip
                        };
                        let v: serde_json::Value = match serde_json::from_slice(&inflated) {
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
                            // v7.1.13: dial failures were INVISIBLE — flux-p2p logged them only
                            // via `tracing` (no subscriber in the TUI), so a client whose every
                            // bootstrap dial fails showed "mesh 0 peers" + "nudging peer" with
                            // zero explanation, forever (the 2026-08-09 Windows stall). Name the
                            // fleet node by IP and put the REASON in the log + stall line.
                            flux_p2p::SwarmAppEvent::DialFailure { peer_id, error } => {
                                let who = peer_id
                                    .map(|p| p.to_string())
                                    .unwrap_or_else(|| "?".into());
                                let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                s.dial_failures = s.dial_failures.saturating_add(1);
                                // Rate-limit: full line each failure is fine (dials back off
                                // 3/6/12s), but keep the stall_reason to the newest.
                                if s.peer_count == 0 {
                                    s.stall_reason = format!("✗ dial failed: {error}");
                                }
                                crate::tlog!("[mesh] ✗DIAL-FAIL {who} — {error}");
                            }
                            flux_p2p::SwarmAppEvent::IncomingConnectionFailed { addr, error } => {
                                crate::tlog!("[mesh] ✗INBOUND-FAIL {addr} — {error}");
                            }
                            _ => {}
                        }
                    }

                    // ── DRAIN completed request results from the spawned tasks ───────────
                    // v7.1.31 (LANE-3 codec-wall wiring): drain everything try_recv() has
                    // ready RIGHT NOW (same non-blocking semantics as before — no new wait),
                    // then decode+precheck the whole batch in PARALLEL across pages BEFORE
                    // the sequential per-item bookkeeping below. Before this, each response's
                    // inflate+bincode-deserialize (decode_verify_backfill) ran one at a time on
                    // this single loop thread even when N chunks landed in the same tick — the
                    // exact "client codec wall" sigil-serve::decode measured at 50,499 blk/s
                    // sequential / 287,960 blk/s rayon-parallel-across-pages (2026-06-27) but
                    // never wired past its own bench (its own doc comment: "verify.rs adopts
                    // these via a thin call-site seam" — this is that seam). All per-item side
                    // effects (bench/commit/stats/tlog) still run sequentially, in original
                    // arrival order, exactly as before — only the CPU-bound decode is batched.
                    let mut last_backfill_time = Instant::now();
                    let mut drained: Vec<(u64, String, Option<Vec<u8>>)> = Vec::new();
                    while let Ok(item) = done_rx.try_recv() {
                        drained.push(item);
                    }
                    let batch_n = drained.len();
                    let decode_t0 = Instant::now();
                    // v7.1.40 (grogu-sync-perf, 2026-08-19): this rayon `.par_iter()` call is
                    // genuinely synchronous CPU-bound work with no `.await` inside it — it runs
                    // straight on whichever tokio worker thread reaches it, blocking that thread
                    // (and everything else scheduled on it: gossip drain, DBG prints, the next
                    // tick's timer) for however long rayon takes to finish. Measured live on a
                    // heavily-loaded box tonight: 9,506 ms for just 5 pages (vs this exact
                    // function's own ~148k blk/s baseline on a quiet box) — rayon's global pool
                    // was itself starved by other processes on the machine, and because this call
                    // wasn't yielding, the WHOLE sync loop froze for the same duration (see memory
                    // project_sigil_sync_index_vs_body_gap for the full trail). Moving it onto
                    // tokio's blocking-pool via `spawn_blocking` doesn't make rayon itself faster
                    // under contention, but it stops a slow decode from freezing everything ELSE
                    // in the loop — network dials, gossip, DBG/verify ticks keep running on the
                    // actual async workers while this one blocking-pool thread waits. `drained` is
                    // moved in and handed back unchanged (nothing else in the loop reads it until
                    // the zip below), so this is a pure isolation change, not a logic change.
                    let (drained, decoded): (
                        Vec<(u64, String, Option<Vec<u8>>)>,
                        Vec<Option<Vec<SigilBlockHeaderV0>>>,
                    ) = if batch_n > 0 {
                        tokio::task::spawn_blocking(move || {
                            use rayon::prelude::*;
                            let decoded = drained
                                .par_iter()
                                .map(|(_, _, bytes)| bytes.as_ref().map(|b| decode_verify_backfill(b)))
                                .collect();
                            (drained, decoded)
                        })
                        .await
                        .unwrap_or((Vec::new(), Vec::new()))
                    } else {
                        (drained, Vec::new())
                    };
                    // Observability for the parallel-decode seam: only logs when the batch is
                    // actually >1 (the case that benefits from rayon fan-out — a batch of 1 is a
                    // no-op either way). Answers "is this seam doing anything on THIS peer/network
                    // right now" without adding noise to the common single-chunk case. Per sigil
                    // skill RULE 0: this is live-path visibility, not a claimed benchmark.
                    if batch_n > 1 {
                        crate::tlog!("[LANE3] batched decode: {batch_n} pages in {} ms", decode_t0.elapsed().as_millis());
                    }
                    for ((start, peer, bytes), headers_precomputed) in
                        drained.into_iter().zip(decoded.into_iter())
                    {
                        inflight = inflight.saturating_sub(1);
                        // v0.58 (10k-sync): keep a successfully-fetched look-ahead range CLAIMED so the
                        // frontier-anchored refill does not re-request it every cycle (~60% serve waste).
                        // Cleared on a genuine miss (empty/timeout) below; pruned once the frontier passes.
                        let mut fetched_ok = false;
                        match bytes {
                            Some(b) => {
                                peer_bench.remove(&peer);              // answered → healthy again
                                let headers = headers_precomputed.unwrap_or_default();
                                let got = headers.len();
                                commit_ring.push_slice(&mut store, &headers);
                                bytes_session += b.len() as u64;
                                if got > 0 {
                                    fetched_ok = true;
                                    lead_n += 1;
                                    // Update continuity with real observed dt for accurate sustained high BW tracking (continuerlighed)
                                    let now = Instant::now();
                                    let dt = now.duration_since(last_backfill_time);
                                    last_backfill_time = now;
                                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                                    s.turbo_continuity.record_success(b.len() as u64, dt);
                                    let chunk_rate = if dt.as_secs_f64() > 0.0 { (b.len() as f64 / dt.as_secs_f64()) * 8.0 } else { 0.0 };
                                    s.turbo_continuity.update_for_continuity(chunk_rate, None, 0, 5);
                                } else { empty_n += 1; }
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
                                // v0.95 FRONTIER-WEDGE: count ONLY responses that delivered REAL
                                // headers for the frontier range (start at/below the next-needed
                                // chunk AND the body actually carried a header ≥ start) yet did not
                                // advance. That is the wedge signature — a forked/unlinkable chunk
                                // the store rejects (got==0 but bytes arrived). It deliberately
                                // EXCLUDES an empty response (a caught-up tip or a lying claim with
                                // no bytes), so the watchdog can't false-fire when we're simply done.
                                // Cleared the instant `synced_to` advances (advance section below).
                                if start <= store.synced_to() + CHUNK
                                    && header_height_range(&b).map_or(false, |(_, mx)| mx >= start)
                                {
                                    frontier_serves_since_advance =
                                        frontier_serves_since_advance.saturating_add(1);
                                }
                                // An EMPTY response over a still-needed range means this peer can't
                                // serve that range (e.g. it pruned genesis / lacks early offsets).
                                // Re-queue to the FRONT (retry promptly) AND bench this peer briefly
                                // so the range rotates to a peer that DOES serve it — otherwise the
                                // lead/genesis chunk round-robins back to the same empty peer forever
                                // and the frontier never advances (v0.10.0 synced-stuck-at-0 bug).
                                let fc = (store.synced_to() / CHUNK) * CHUNK;
                                // v7.0.18: distinguish EMPTY (no headers — peer lacks the range)
                                // from REJECTED (real headers arrived but OUR store refused to
                                // splice them — a poisoned local seam). Benching the peer on a
                                // reject punishes an honest peer for our own bad block; on a
                                // single-peer mesh that starved sync completely (bench 10s →
                                // re-request → reject → bench …). Rejects are the self-heal's
                                // job; only a genuinely header-less reply benches the peer.
                                let had_headers = header_height_range(&b)
                                    .map_or(false, |(_, mx)| mx >= start);
                                if got == 0 && start >= fc && !had_headers {
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
                        // v0.58 (10k-sync): only a genuine miss frees the claim for re-request.
                        if !fetched_ok { assigned.remove(&start); }
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
                    // LANE-C write-through: drain the ring to durable storage BEFORE the frontier
                    // cursor re-reads synced_to(), so buffered heights are never re-fetched.
                    commit_ring.flush(&mut store);
                    {
                        let pb = state.lock().unwrap_or_else(|e| e.into_inner()).peer_best_height;
                        let deep_gap = store.synced_to().saturating_add(CHUNK.saturating_mul(4)) < pb;
                        if deep_gap && !bulk_armed { commit_ring.arm(&store); bulk_armed = true; }
                        else if !deep_gap && bulk_armed { commit_ring.finish(&mut store); store.set_bulk_load(false); bulk_armed = false; }
                    }
                    // v6.0.0: publish the live durable-commit rate to the TUI (the ⚡ readout).
                    {
                        let committed_now = commit_ring.committed();
                        let dt = last_commit_t.elapsed().as_secs_f64();
                        if dt >= 0.5 {
                            let rate = committed_now.saturating_sub(last_committed) as f64 / dt;
                            last_committed = committed_now; last_commit_t = Instant::now();
                            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                            s.commit_rate = rate; s.turbo_armed = bulk_armed;
                        }
                    }
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
                    // v0.96 SUB-1s COLD START — SMART RETRY: until the first headers land
                    // (`fetched_session == 0`) probe FAST (200ms) and HEDGE across EVERY healthy
                    // peer with a SHORT (1.2s) timeout. The first reply seeds peer_best AND lands
                    // up to ~8192 free headers, so a single slow/dead peer (Delta going down, or a
                    // peer wedged under the connection storm) can no longer hold the whole backfill
                    // at "0 blk/s" for up to REQ_TIMEOUT(=10s). Once anything has landed, revert to
                    // the cheap single-best-peer 500ms cadence with the tolerant 10s WAN timeout.
                    let cold_start = fetched_session == 0;
                    let probe_interval = if cold_start { Duration::from_millis(200) } else { PROBE_EVERY };
                    if last_probe.elapsed() >= probe_interval {
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
                        if !healthy.is_empty() {
                            // v0.35 (DeepSeek audit S5): stamp the timer ONLY when a probe is
                            // actually SENT (peers connected). Stamping before the peer check
                            // BURNED the overdue first probe pre-bootstrap, delaying the real
                            // first probe by an extra interval after the first PeerConnected.
                            last_probe = Instant::now();
                            // LANE-P v0.59 STALL-BREAKER: normally probe the floor-aligned
                            // frontier_chunk (cache-friendly look-ahead). But if the contiguous
                            // frontier hasn't advanced for a while, request the EXACT next-needed
                            // height [synced_to..] so the lead block lands and synced_to moves.
                            let probe_from = if last_advance_t.elapsed() >= Duration::from_secs(6) {
                                store.synced_to()
                            } else {
                                frontier_chunk
                            };
                            // COLD → hedge across ALL healthy peers (first reply wins). WARM →
                            // the single best non-behind peer (v0.38.1: a behind peer answers
                            // EMPTY for [frontier, MAX], wasting a round-trip and seeding nothing).
                            let targets: Vec<_> = if cold_start {
                                healthy.clone()
                            } else {
                                let fc = frontier_chunk;
                                healthy.iter().find(|p| match peer_top.get(&p.to_string()) {
                                    Some(&(top, seen)) => top + CHUNK >= fc || now.duration_since(seen).as_secs() > 4,
                                    None => true,
                                }).or_else(|| healthy.first()).copied().into_iter().collect()
                            };
                            let probe_timeout = if cold_start { Duration::from_millis(1200) } else { REQ_TIMEOUT };
                            for peer in targets {
                                let payload = serde_json::to_vec(
                                    &BackfillReq { from: probe_from, to: u64::MAX, headers_only: true, codec: 1 }
                                ).unwrap();
                                let n = net.clone();
                                let tx = probe_tx.clone();
                                tokio::spawn(async move {
                                    let r = tokio::time::timeout(probe_timeout, n.send_request(peer, payload)).await;
                                    if let Ok(Ok(b)) = r { let _ = tx.send(b); }
                                });
                            }
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
                        // Continuity X: prefer high-BW momentum peer for sustained download rate
                        let best_peer: Option<String> = {
                            // select_best_peer wants &[PeerId]; healthy is peer-id strings, and
                            // peer_momenta is unpopulated (update_for_continuity is called with
                            // peer=None), so this preference is advisory and currently always None
                            // → round-robin below is the live path. Parse to PeerId for the call,
                            // map the result back to String to match the String peer flow.
                            let s = state.lock().unwrap_or_else(|e| e.into_inner());
                            let pids: Vec<_> = healthy.iter().filter_map(|p| p.to_string().parse().ok()).collect();
                            s.turbo_continuity.select_best_peer(&pids).map(|p| p.to_string())
                        };
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
                        // Turbo Sync X + continuity: bias to high-BW momentum peer for continuous high download bandwidth (no drops in rate)
                        let best_peer: Option<String> = {
                            let s = state.lock().unwrap_or_else(|e| e.into_inner());
                            let pids: Vec<_> = healthy.iter().filter_map(|p| p.to_string().parse().ok()).collect();
                            s.turbo_continuity.select_best_peer(&pids).map(|p| p.to_string())
                        };
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
                            let full_archive_mode = !recent_only_rt.load(Ordering::Relaxed);
                            // v0.58 (10k-sync fix): full-archive must PIPELINE look-ahead like recent-only.
                            // The `1` cap (a v0.10.0 frontier-stall over-correction) serialized the frontier to
                            // ONE server serve per round-trip -> inflight collapsed to 1, frontier parked at
                            // 1+3*32768=98305. CommitBuffer.push_slice ACCUMULATES and commit_batch_durable
                            // SORTS by height, so contiguous parallel look-ahead commits in ascending order
                            // (strict-downward-linkage holds); i==0 (exact synced_to) is issued FIRST each
                            // cycle with redundant fanout so the lead chunk always chains.
                            let refill_slots = (max_inflight as u64).saturating_mul(3); // v0.58: scan wide so fetched-but-claimed look-ahead is skipped yet fresh ranges still fill inflight
                            for i in 0..refill_slots {
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
                                // v0.58 (10k-sync): bound the look-ahead so claimed/retained memory stays bounded.
                                if i > 0 && start > store.synced_to().saturating_add(CHUNK.saturating_mul(16)) { break; }
                                if !assigned.insert(start) { continue; }  // in flight OR already fetched (claimed)
                                let fanout = if i == 0 {
                                    if full_archive_mode {
                                        healthy.len().max(1)
                                    } else {
                                        FRONTIER_REDUNDANCY.min(healthy.len()).max(1)
                                    }
                                } else {
                                    1
                                };
                                // Turbo Sync X + BandwidthContinuity (continuerlighed for continuous high download bandwidth):
                                // compute once per batch using PID/Kalman/Momentum to choose dynamic chunk size and
                                // boosted parallel (X) factor to keep the network pipe full at high sustained rate,
                                // avoiding stalls and idle gaps.
                                // Compute real observed bps from session for accurate PID/Kalman feedback (continuous high BW)
                                let elapsed = loop_start.elapsed().as_secs_f64().max(0.1);
                                let observed_bps = (bytes_session as f64 / elapsed) * 8.0; // rough to bps
                                let (use_chunk, eff_fan, best_from_cont) = {
                                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                                    // Use predicted_rate from Kalman for smarter chunk to maintain continuous high BW
                                    let (c, x, bp, predicted) = s.turbo_continuity.update_for_continuity(
                                        observed_bps, None, CHUNK as u64 * 1024, 12);
                                    let pid_rate = s.turbo_continuity.pid.get_rate();
                                    let base_ch = if c > 0 { (c / 1024) as u64 } else { CHUNK };
                                    // Use pid_rate to boost chunk and fan for high continuous BW: if low, increase parallel to catch up, if high, sustain
                                    let rate_factor = if pid_rate < 40.0 { 1.5 } else if pid_rate > 60.0 { 0.8 } else { 1.0 };
                                    let ch = if predicted > 10_000_000.0 { ((base_ch as f64) * rate_factor * 1.2) as u64 } else { (base_ch as f64 * rate_factor) as u64 };
                                    let eff_x = ((x as f64) * rate_factor).max(1.0).min(fanout as f64) as usize;
                                    (ch, eff_x, bp.map(|p| p.to_string()))
                                };
                                // Pace requests smoothly using PID rate for continuous high BW (smooth flow, no bursts that drop effective rate)
                                // use PID rate vs target for proportional delay: if current low, shorter delay to catch up to high continuous rate
                                let (target_delay, mut last_send) = {
                                    let s = state.lock().unwrap_or_else(|e| e.into_inner());
                                    let pid_rate = s.turbo_continuity.pid.get_rate().max(1.0);
                                    let target_rate = 50.0;
                                    let delay_ms = (20.0 * (target_rate / pid_rate)) as u64; // base 20ms at 50/s, scaled
                                    (Duration::from_millis(delay_ms.min(100).max(1)), Instant::now())
                                };
                                for k in 0..eff_fan {
                                    if inflight >= max_inflight { break; }
                                    let elapsed = last_send.elapsed();
                                    if elapsed < target_delay {
                                        tokio::time::sleep(target_delay - elapsed).await;
                                    }
                                    last_send += target_delay; // accumulate for steady continuous rate
                                    let peer = if k == 0 {
                                        best_from_cont.as_ref().and_then(|bs| healthy.iter().find(|p| p.to_string() == *bs).cloned()).unwrap_or_else(|| healthy[(rr + k) % healthy.len()].clone())
                                    } else {
                                        healthy[(rr + k) % healthy.len()].clone()
                                    };
                                    let payload = serde_json::to_vec(
                                        &BackfillReq { from: start, to: start + CHUNK, headers_only: true, codec: 1 } // v0.58: span==stride(CHUNK) so look-ahead TILES contiguous (was use_chunk -> gaps)
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
                                rr = rr.wrapping_add(eff_fan);
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
                        // v0.58 (10k-sync): drop claims the frontier has passed (stored) — bounds `assigned`
                        // and always lets the exact frontier chunk re-issue. Stall backstop: if the frontier
                        // is wedged, clear ALL claims so any range dropped from the bounded retain buffer is
                        // re-fetched (prevents a permanent gap).
                        assigned.retain(|&s| s >= now_synced);
                        if last_advance_t.elapsed() >= Duration::from_secs(6) { assigned.clear(); }
                        // DYNAMIC BASE: the lowest servable height creeps UP as producers prune early
                        // history from their RAM window (the disk range-serve of pruned-low ranges is
                        // unreliable). If the frontier chunk stays unservable by ALL peers for ≥5s
                        // while we're below the tip, skip it: advance `base` one chunk and re-anchor.
                        // Self-heals to whatever the mesh actually serves (verified spine then anchors
                        // at the lowest servable height, honestly — not necessarily genesis).
                        if now_synced > last_synced_seen {
                            last_synced_seen = now_synced;
                            last_advance_t = Instant::now();
                            frontier_serves_since_advance = 0; // v0.95: real progress clears wedge evidence
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
                        let peer_count_now = net.peer_count();
                        s.peer_count = peer_count_now;
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
                        s.stall_reason = if peer_count_now == 0 && loop_start.elapsed() >= Duration::from_secs(6) {
                            "no peers — mesh not grafted (bootstrap/dialing; no backfill peer available)".into()
                        } else if net_tip > now_synced && stalled_for >= Duration::from_secs(6) {
                            format!("no advance {}s @ {} (gap {}) — retrying exact [{}..] from a rotating peer",
                                stalled_for.as_secs(), now_synced, net_tip.saturating_sub(now_synced), now_synced)
                        } else {
                            String::new()
                        };
                        s.last_message_at = Some(Instant::now());
                        // Feed delta rate to continuity every state tick for PID to sustain continuous high BW (better than cumulative)
                        let now_t = Instant::now();
                        let dt = now_t.duration_since(last_rate_time).as_secs_f64().max(0.01);
                        let delta_b = bytes_session.saturating_sub(last_rate_bytes);
                        let rate_bps = (delta_b as f64 / dt) * 8.0;
                        last_rate_time = now_t;
                        last_rate_bytes = bytes_session;
                        s.turbo_continuity.update_for_continuity(rate_bps, None, 0, 5);
                        // also feed pid current rate for sustained to keep controller driving the continuous target
                        let pid_rate_bps = s.turbo_continuity.pid.get_rate() * 1_000_000.0;
                        s.turbo_continuity.update_for_continuity(pid_rate_bps, None, 0, 5);
                    }

                    // ── PASS 2: background full-archive body-backfill (archive.rs) ──────────
                    // Trustless convergence to a FULL archive (#156): walk the verified skeleton,
                    // fetch the next missing body range, store a body ONLY if it hashes to the
                    // skeleton's committed block_hash. Low-priority: single in-flight deep-gap
                    // request, 500ms throttle, and a no-op until the skeleton has records (pass-1
                    // landed) - so it never competes with the frontier during normal sync.
                    if pass2_env && !recent_only_rt.load(Ordering::Relaxed) {
                        while let Ok((gap_from, bytes)) = pass2_rx.try_recv() {
                            pass2_inflight = false;
                            if let (Some(b), Some(sk)) = (bytes, skel.as_mut()) {
                                let bodies = decode_verify_backfill(&b);
                                if bodies.is_empty() && b.len() > 1 {
                                    // Non-empty reply but nothing decoded/precheck'd — most likely the
                                    // 64 MiB decode cap (see pass2_chunk's doc comment above). Requesting
                                    // this exact gap again at the same width would fail identically
                                    // forever, so halve the window (floor 64) to guarantee progress.
                                    pass2_chunk = (pass2_chunk / 2).max(64);
                                    crate::tlog!("[pass2] gap@{} decode/precheck empty from a {} B reply — shrinking pass2_chunk to {}",
                                        gap_from, b.len(), pass2_chunk);
                                } else if !bodies.is_empty() {
                                    // Recovered — grow back gradually, never past the CHUNK ceiling.
                                    pass2_chunk = (pass2_chunk.saturating_mul(3) / 2).min(CHUNK);
                                }
                                // V7-INGEST: route verified bodies through the SST-ingest sink when
                                // SIGIL_DB_SST_INGEST is on; flush per reply so committed==fetched
                                // before next_body_gap walks the gap (no re-fetch). Flag off →
                                // byte-identical legacy per-chunk commit.
                                let (stored, rejected) = match pass2_sink.as_mut() {
                                    Some(sink) => {
                                        let (st, rj) = sink.accept(sk, &mut store, &bodies);
                                        (st + sink.flush(&mut store), rj)
                                    }
                                    None => archive::ingest_bodies_verified(sk, &mut store, &bodies),
                                };
                                if stored > 0 || rejected > 0 {
                                    store.advance();
                                    crate::tlog!("[pass2] gap@{} stored={} rejected={} archive={:.1}%",
                                        gap_from, stored, rejected,
                                        archive::archive_fraction(sk, &store, sync_base) * 100.0);
                                }
                            }
                        }
                        if !pass2_inflight && last_pass2.elapsed() >= Duration::from_millis(500) {
                            last_pass2 = Instant::now();
                            let gap = skel.as_ref().and_then(|sk| archive::next_body_gap(sk, &store, sync_base, pass2_chunk));
                            if let Some((from, to)) = gap {
                                if let Some(peer) = net.connected_peers().first().cloned() {
                                    pass2_inflight = true;
                                    let payload = serde_json::to_vec(
                                        &BackfillReq { from, to, headers_only: true, codec: 1 }
                                    ).unwrap();
                                    let n = net.clone();
                                    let tx = pass2_tx.clone();
                                    tokio::spawn(async move {
                                        let r = tokio::time::timeout(REQ_TIMEOUT, n.send_request(peer, payload)).await;
                                        let _ = tx.send((from, r.ok().and_then(|x| x.ok())));
                                    });
                                }
                            }
                        }
                    }

                    // ── VCATCH: skeleton/anchor-independent body backfill (see decl comment) ──
                    if vcatch_env && !recent_only_rt.load(Ordering::Relaxed) {
                        while let Ok((gap_from, bytes)) = vcatch_rx.try_recv() {
                            vcatch_inflight = false;
                            if let Some(b) = bytes {
                                let headers = decode_verify_backfill(&b);
                                let got = headers.len();
                                if got > 0 {
                                    // NOT commit_ring.push_slice: that path treats anything below
                                    // store.synced_to() as "already stored" and silently drops it
                                    // (commit.rs:175-186, correct for the classic frontier-advancing
                                    // case) — but synced_to is exactly the inflated, index-only
                                    // watermark this mechanism exists to work AROUND, so every
                                    // VCATCH page landed below it and was discarded (measured live:
                                    // "got=10001 headers" every 500ms, verified_to never moved).
                                    // put_blocks_batch writes directly, no synced_to-relative skip.
                                    let stored = store.put_blocks_batch(&headers);
                                    store.advance();
                                    crate::tlog!("[vcatch] gap@{} got={} stored={} verified_to={} synced_to={}",
                                        gap_from, got, stored, store.verified_to(), store.synced_to());
                                }
                            }
                        }
                        if !vcatch_inflight && last_vcatch.elapsed() >= Duration::from_millis(500) {
                            last_vcatch = Instant::now();
                            let v = store.verified_to().max(store.base());
                            let s = store.synced_to();
                            // Only reach past the synced frontier, and only when there's a REAL gap
                            // worth a request (avoids spamming a 1-block gap every 500ms at the tip).
                            if s > v.saturating_add(CHUNK / 4) {
                                if let Some(peer) = net.connected_peers().first().cloned() {
                                    vcatch_inflight = true;
                                    let to = (v + CHUNK).min(s);
                                    let payload = serde_json::to_vec(
                                        &BackfillReq { from: v, to, headers_only: true, codec: 1 }
                                    ).unwrap();
                                    let n = net.clone();
                                    let tx = vcatch_tx.clone();
                                    tokio::spawn(async move {
                                        let r = tokio::time::timeout(REQ_TIMEOUT, n.send_request(peer, payload)).await;
                                        let _ = tx.send((v, r.ok().and_then(|x| x.ok())));
                                    });
                                }
                            }
                        }
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
                        // Real rate feedback to continuity for sustained high BW (kontinuerlighed)
                        let observed_bps = (bytes_session as f64 / upt as f64) * 8.0; // rough bps
                        {
                            let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                            // Update with observed to let PID/Kalman adjust for continuous high rate, no drops
                            let _ = s.turbo_continuity.update_for_continuity(observed_bps, None, 0, 10);
                        }
                        crate::tlog!(
                            "[DBG] up={upt}s synced={synced_now} tip={pbest} gap={gap} | reqs={req_n} lead={lead_n}({:.0}%) empty={empty_n} timeout={timeout_n}({:.0}%) | inflight={inflight} assigned={} fetched={fetched_session} bytes={}MB | peers={peers}(mesh {mesh}, healthy {hpeers}) tip_age={tip_age}s base={} | cont_score={:.2} sustained={:.1}MB/s pid_rate={:.1}",
                            lead_n as f64 / win as f64 * 100.0,
                            timeout_n as f64 / win as f64 * 100.0,
                            assigned.len(), bytes_session / 1_048_576, store.base(),
                            { let s = state_clone.lock().unwrap_or_else(|e| e.into_inner()); s.turbo_continuity.continuity_score },
                            { let s = state_clone.lock().unwrap_or_else(|e| e.into_inner()); s.turbo_continuity.sustained_rate_bps / 1_000_000.0 },
                            { let s = state_clone.lock().unwrap_or_else(|e| e.into_inner()); s.turbo_continuity.pid.get_rate() }
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
                        //
                        // ⚠️ REGRESSION FIX #2 (2026-08-16): `base_g <= 1` was ALSO the only gate
                        // for even ATTEMPTING this check — which is a chicken-and-egg trap for a
                        // full-sync client whose `base` already crept forward under a chain that
                        // has SINCE died. Such a client can never satisfy `base_g <= 1` again (base
                        // doesn't move backward on its own), so it can never re-examine genesis and
                        // never self-heals — it just sits re-requesting a chunk range from the dead
                        // chain forever. Operator-reported live 2026-08-16: a sigil-top synced
                        // ~36M blocks against the pre-reset chain, then the network reset to a fresh
                        // genesis at height 687K+; the client stayed stuck showing
                        // "chunk [36,113,830..36,115,878]" — a range that will never exist on the
                        // new chain — because `base` (36M+something) never got back down to ≤1 to
                        // even trigger the check that would have caught this.
                        //
                        // Fix: check the FIXED genesis-anchor height (1) instead of wherever `base`
                        // currently sits. Height 1 never moves, so this is a strictly more general
                        // form of the same comparison the code above already documents as sound for
                        // full-sync mode — a no-op for a healthy client (base is already 1 there, so
                        // this examines the exact same block as before) and now ALSO correct for a
                        // stale one. `has_height(GENESIS_ANCHOR_HEIGHT)` guards the light/recent-
                        // window case exactly as `has_height(base_g)` did before: a client that
                        // legitimately never downloaded genesis just no-ops here, unchanged.
                        const GENESIS_ANCHOR_HEIGHT: u64 = 1;
                        let base_g = store.base();
                        let mut genesis_reset = false;
                        if store.has_height(GENESIS_ANCHOR_HEIGHT) {
                            if let Some(hdr) = store.get_header_at_height(GENESIS_ANCHOR_HEIGHT) {
                                if store.note_genesis(&hex::encode(hdr.hash())) {
                                    genesis_reset = true;
                                    crate::tlog!("[sync] LANE-S: genesis CHANGED (checked at height {GENESIS_ANCHOR_HEIGHT}, local base was {base_g}) → wiped stale watermarks, self-healing to the fresh chain");
                                    clear_persisted_tip(); // LANE-S (b): drop the pre-reset cached tip
                                    let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                    s.peer_best_height = 0;
                                    s.verified = 0;
                                    s.blocks_synced = 0;
                                    s.reset_pending = false; // the wipe is done in-line here
                                    drop(s);
                                    // reset_watermarks() (inside note_genesis) zeroes `base` along
                                    // with everything else, but nothing else in this reset path was
                                    // re-establishing the genesis anchor `set_base`'s own doc comment
                                    // says to set "once after open, before sync" — a live reset IS
                                    // that same "before sync" moment again. Without this, `base`
                                    // stays 0 (SIGIL's true height-0 genesis, which isn't
                                    // backfill-servable per the comment at `set_base`'s definition)
                                    // instead of 1, the actual anchor full-sync mode expects.
                                    store.set_base(GENESIS_ANCHOR_HEIGHT);
                                }
                            }
                        }
                        // ── LANE-S v2 (2026-08-16): block-content-independent fallback ─────────
                        // The check above depends on `has_height(GENESIS_ANCHOR_HEIGHT)` — a
                        // client whose local store no longer separately retains the
                        // genesis-adjacent block (entirely plausible after syncing tens of
                        // millions of blocks under a chain that's since died — nothing in this
                        // codebase guarantees height 1 survives that) silently no-ops there and
                        // never resets. Operator-reported live 2026-08-16: v7.1.22 (which shipped
                        // the fix above) did NOT resolve the stall even once "mesh 1 peers" was
                        // confirmed live — proving that fix alone is insufficient for this client.
                        //
                        // This is a strictly more general check that needs no locally-stored
                        // block at all: compare our claimed progress directly against what a LIVE,
                        // currently-connected peer is reporting RIGHT NOW. An honest peer cannot
                        // report a height dramatically LOWER than what we supposedly already
                        // verified — if it does, our local state describes a chain that no longer
                        // exists, full stop, independent of which specific blocks we still hold.
                        // Requires an oracle-CONFIRMED reading (not a raw disk-cache seed) and a
                        // generous 1000-block margin so this can never fire on ordinary lag.
                        //
                        // ⚠️ BUG FIX (2026-08-16, same day, next attempt): the first cut of this
                        // check compared `base_g` (`store.base()`) — WRONG FIELD. In full-sync
                        // mode `base` is the STABLE genesis anchor and stays ~1 by design (see the
                        // v0.70→v0.71.x comment above); it is `store.synced_to()` — what actually
                        // drives the displayed `sync_cursor`/chunk range — that creeps forward and
                        // is what stays stuck post-reset. Comparing `base_g` (~1) against a live
                        // peer height in the hundreds of thousands could never exceed the +1000
                        // margin, so this check could NEVER fire, which is exactly why the
                        // operator's stall persisted through this fix too. Proven live: four
                        // consecutive releases (v7.1.21-24) with a growing set of fixes, zero
                        // observed change in the operator's frozen chunk display, until this line
                        // was corrected.
                        let synced_g = store.synced_to();
                        if !genesis_reset {
                            let (peer_confirmed, peer_reported) = {
                                let s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                (s.peer_best_oracle_confirmed, s.peer_best_height)
                            };
                            if peer_confirmed && peer_reported > 0
                                && synced_g > peer_reported.saturating_add(1000)
                            {
                                crate::tlog!(
                                    "[sync] LANE-S v2: local synced_to {synced_g} (base {base_g}) is \
                                     impossibly far above the live peer's confirmed height \
                                     {peer_reported} → stale post-reset state, wiping (content-independent path)"
                                );
                                store.reset_watermarks();
                                store.set_base(GENESIS_ANCHOR_HEIGHT);
                                clear_persisted_tip();
                                let mut s = state_clone.lock().unwrap_or_else(|e| e.into_inner());
                                s.peer_best_height = peer_reported; // keep what we just learned, don't zero it
                                s.peer_best_oracle_confirmed = true;
                                s.verified = 0;
                                s.blocks_synced = 0;
                                s.reset_pending = false;
                                drop(s);
                                genesis_reset = true; // reuse the flag: also skips LANE-B this tick
                            }
                        }
                        if !genesis_reset {
                        // LANE-B prefix multiplier — trusted-checkpoint fast-forward (fail-loud,
                        // idempotent per tick). No-op until dns_anchor_tip() returns a fresh,
                        // SQIsign-verified (height,hash); verify_to_parallel below always guarantees
                        // correctness, so this is zero-regression until dns_anchor_tip() is real.
                        //
                        // v7.1.37 fix: dns_anchor_tip()'s epoch is a GLOBAL one-shot resource shared
                        // with LANE-A (snapshot-pull) — whichever lane calls it first burns the epoch
                        // for both, even on a doomed attempt. LANE-B ran unconditionally every tick and
                        // fired on tick 1 before any peer had even connected, immediately failing
                        // ("anchor block not stored — cannot authenticate checkpoint", true for ANY
                        // fresh node since the anchor references a height far beyond genesis) and
                        // permanently starving LANE-A — the lane actually suited to a fresh sync — of
                        // its one real chance. Defer to LANE-A's window: only let LANE-B spend the
                        // anchor once LANE-A has already had its shot (snapshot_attempted) or can no
                        // longer apply (synced_to has moved past sync_base, its own fresh-node gate).
                        let lane_a_window_closed = snapshot_attempted || store.synced_to() > sync_base;
                        if lane_a_window_closed {
                            if let Some((anchor_h, anchor_hash)) = dns_anchor_tip().await {
                                match verify::fast_forward_to_anchored_checkpoint(
                                    &mut store, anchor_h, &anchor_hash, verify::DEFAULT_FRONTIER_WINDOW,
                                ) {
                                    Ok(rep) => crate::tlog!("[sync] LANE-B fast-forward: trusted h={} bulk_below={} verified_to={} frontier_checked={}",
                                        rep.trusted_height, rep.bulk_trusted_below, rep.verified_to, rep.frontier_checked),
                                    Err(e) => crate::tlog!("[sync] LANE-B fast-forward refused ({e}) — verify_to_parallel covers it"),
                                }
                            }
                        }
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
                        let mut heal_pending = false; // set by the self-heal arm inside the closure
                        let verify_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let vt0 = std::time::Instant::now();
                            let report = crate::chain_verify::verify_to_parallel(&mut store, VERIFY_BUDGET);
                            if report.checked > 0 {
                                let secs = vt0.elapsed().as_secs_f64().max(0.001);
                                crate::tlog!("[verify] parallel pass: checked {} → verified_to={} in {} ms ({:.0} blk/s)",
                                    report.checked, report.verified_to, vt0.elapsed().as_millis(), report.checked as f64 / secs);
                            }
                            let class = crate::gap_sync::classify_break(&report);
                            // v7.0.22: background flush — the inline call held THIS apply
                            // loop 1-4s per tick (measured), the periodic "rate 0" dip.
                            store.flush_background();

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
                                    // v0.95 FRONTIER-WEDGE: real frontier chunks keep arriving but
                                    // won't splice (forked chunk the store rejects, or — pre-strict-
                                    // ingest — an out-of-order squatter) → `best` stays == `frontier`,
                                    // so the best>frontier hole test goes blind and the run would die
                                    // on the generic timeout. `frontier_active` is the wedge proof (real
                                    // headers received without advance); the shared watchdog_verdict
                                    // turns it into a LOUD named failure (exit 4). Gated on peers>0 so a
                                    // dead mesh shows the peers=0 stall_reason instead, and on real
                                    // received bytes so a caught-up tip / lying claim never trips it.
                                    const FRONTIER_WEDGE_SERVES: u32 = 4;
                                    let frontier_active = s.peer_count > 0
                                        && frontier_serves_since_advance >= FRONTIER_WEDGE_SERVES;
                                    match crate::gap_sync::watchdog_verdict(
                                        frontier, best, frontier_active, stalled,
                                        Duration::from_secs(watchdog_secs),
                                    ) {
                                        // v7.0.14 LIGHT-MONITOR FALSE-BREAK FIX: in light-monitor
                                        // mode the node HOLDS NOTHING and tracks the tip via the
                                        // recent-window snap — there is no genesis-contiguous frontier
                                        // to wedge. After [F] flips full-archive → light, the frontier
                                        // is parked at the ABANDONED full-archive height (e.g. 8.97M);
                                        // the wedge watchdog would false-fire "SPINE BREAK — STUCK"
                                        // forever even while gap-to-tip is 25 and rate is 9 blk/s.
                                        // Light-monitor health = gap-to-tip + rate (surfaced above),
                                        // NOT this frontier watchdog. (A truly Fatal/corrupt-header
                                        // break above still fires — only the frontier WEDGE is gated.)
                                        Some(_) if s.light_mode => {
                                            s.sync_failure = None;
                                            failure_announced = false;
                                        }
                                        // v7.0.18 FRONTIER SELF-HEAL: real headers keep arriving for
                                        // the frontier yet refuse to splice → the poisoned block is
                                        // OURS (a bulk/skeleton lane wrote an unlinkable seam; that
                                        // lane skips linkage checks). Fetching harder can never fix
                                        // it — the live wedge showed 2M headers fetched at 6.5k blk/s
                                        // with the spine parked. Roll OUR frontier back and refetch
                                        // clean. Bounded: after MAX_HEALS the loud verdict stands.
                                        Some(f) if frontier_active && heal_attempts < 3 => {
                                            heal_attempts += 1;
                                            let newf = store.rollback_frontier(4096);
                                            crate::tlog!("[sync] 🔧 frontier WEDGE self-heal #{heal_attempts} — honest headers refuse to splice at h={frontier} ({}); rolled local frontier back to h={newf}, refetching clean", f.reason);
                                            s.sync_failure = None;
                                            failure_announced = false;
                                            heal_pending = true;
                                        }
                                        Some(f) => {
                                            if !failure_announced {
                                                failure_announced = true;
                                                let kind = if best > frontier { "STALL" } else { "WEDGE" };
                                                crate::tlog!("[sync] ✗ SPINE BREAK ({kind}) — {}", f.reason);
                                            }
                                            s.sync_failure = Some((f.height, f.reason));
                                        }
                                        None => {
                                            // advancing, or genuinely caught up to what peers serve →
                                            // clear any prior stall + re-arm the announcer.
                                            s.sync_failure = None;
                                            failure_announced = false;
                                            heal_attempts = 0; // real progress re-arms the self-heal budget
                                        }
                                    }
                                }
                            }
                        }));
                        if verify_res.is_err() {
                            crate::tlog!("[sync] ⚠ verify/watchdog tick PANICKED — recovered (sync thread alive, mutex un-poisoned); continuing");
                        }
                        if heal_pending {
                            // v7.0.18: the self-heal rolled the store's frontier back — clear every
                            // in-flight claim and bench so the refill re-requests from the NEW
                            // frontier immediately, and re-arm the watchdog clocks so the heal gets
                            // a full window to prove itself before the next verdict.
                            assigned.clear();
                            peer_bench.clear();
                            frontier_serves_since_advance = 0;
                            last_synced_seen = store.synced_to();
                            last_advance_t = Instant::now();
                            last_verified_seen = store.verified_to();
                            last_verified_advance_t = Instant::now();
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
                        // v7.1.33 (grogu-sync-perf): ROOT-CAUSE gate, not just a bound. synced_to
                        // can be near peer_best the INSTANT a LANE-S trust-jump fires, long before
                        // [lo, hi] has any real stored data (a fresh reset: verified_to resets to 0
                        // in reset_watermarks() and only advances via genuine sequential download+
                        // verify — unlike synced_to, it can't be trust-jumped). Require verified_to
                        // to have gotten past `lo` before even trying the scan below: that's the
                        // actual signal that [lo, ..] has real content worth hashing, not just a
                        // number that happens to be close to the peer's claimed tip. The ATTEMPT_CAP
                        // bound inside the scan (below) stays as defense-in-depth for whatever this
                        // gate doesn't catch, not the primary fix anymore.
                        let has_real_data_near_lo = store.verified_to() > lo;
                        if hi > lo && has_real_data_near_lo {
                            // v0.28: batch-cache the window ONCE (kills the per-header DB-read
                            // bottleneck that capped pos at ~190 blk/s), then re-verify the
                            // in-memory batch with BLAKE every tick — a real useful-hashrate.
                            //
                            // v7.1.32 (grogu-sync-perf, closing the "sync goes silent 60s+" finding,
                            // swarm msg #72): `at_tip_idle` goes true the INSTANT synced_to is near
                            // peer_best, which a LANE-S v2 trust-jump makes true IMMEDIATELY — long
                            // before [lo, hi] has any actually-stored headers (a fresh-reset node has
                            // base≈1, synced_to≈peer_best, nothing downloaded in between yet).
                            // Unbounded, this loop walked the ENTIRE [lo, hi] range one DB get at a
                            // time hunting for 8192 hits that don't exist — live-measured at up to
                            // ~35s of the WHOLE sync loop blocked (refill/drain/everything) for a
                            // SINGLE tick on this shared box (DB gets under real concurrent load
                            // measured ~3.5ms each here, not the sub-µs a quiet box would give — so
                            // even the original 4096-miss bound cost ~14s). Two bounds now: (1) a
                            // much smaller per-tick miss cap so one attempt is cheap even under
                            // contention, (2) a 3s backoff after an all-empty attempt so a known-
                            // sparse range isn't immediately re-hammered next tick. Neither changes
                            // behavior once real data exists (a hit resets the miss streak; any hit
                            // clears the backoff via pos_window no longer being empty).
                            let scan_due = pos_scan_empty_since
                                .map_or(true, |t| t.elapsed() >= Duration::from_secs(3));
                            if (pos_window_base != lo || pos_window.is_empty()) && scan_due {
                                pos_window.clear();
                                pos_bytes.clear();
                                let mut h = lo;
                                const MISS_CAP: u32 = 200;
                                // v7.1.33 (grogu-sync-perf): the consecutive-miss cap alone left a
                                // gap — live-measured a 36s tick STILL happening under
                                // SIGIL_SYNC_INFLIGHT=16 (more concurrent backfill => hits scattered
                                // sparsely-but-often-enough through the range to keep resetting the
                                // miss streak below MISS_CAP without ever tripping it, so the walk
                                // still ran most of the way to `hi`). Bound total attempts too,
                                // independent of the hit/miss pattern — worst case is now ATTEMPT_CAP
                                // DB gets no matter how the hits are distributed. A capped-out window
                                // is used as-is (whatever was found) rather than treated as "empty
                                // and retry" — this only shrinks how much of a huge/sparse range gets
                                // hashed per tick, it never blocks.
                                const ATTEMPT_CAP: u64 = 2_000; // defense-in-depth now that has_real_data_near_lo gates the common case
                                let mut consecutive_misses: u32 = 0;
                                let mut attempts: u64 = 0;
                                while h < hi && pos_window.len() < 8192 && attempts < ATTEMPT_CAP {
                                    attempts += 1;
                                    if let Some(hdr) = store.get_header_at_height(h) {
                                        pos_bytes.extend_from_slice(hdr.hash().as_ref()); // derive each header hash ONCE (the serde cost, amortized)
                                        pos_window.push(hdr);
                                        consecutive_misses = 0;
                                    } else {
                                        consecutive_misses += 1;
                                        if consecutive_misses >= MISS_CAP { break; }
                                    }
                                    h += 1;
                                }
                                pos_window_base = lo;
                                pos_scan_empty_since =
                                    if pos_window.is_empty() { Some(Instant::now()) } else { None };
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
                        // v7.1.32 (grogu-sync-perf): permanent slow-tick guard. This single tick
                        // body does everything (gossip drain, tipfetch, refill, LANE-S, the idle
                        // scan above) synchronously — a real live stall here measured up to ~35s
                        // for ONE tick before this session's fixes (see the MISS_CAP comment
                        // above), and was completely invisible until independently instrumented.
                        // A generous 500ms threshold (normal ticks are single-digit ms) means this
                        // never fires in healthy operation but catches the NEXT unbounded-scan-
                        // shaped bug immediately instead of needing another debugging session.
                        if tick_t0.elapsed() >= Duration::from_millis(500) {
                            crate::tlog!("[sync] ⚠ SLOW TICK: {} ms (idle-branch) — sync loop was blocked this long, nothing else could progress", tick_t0.elapsed().as_millis());
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await; // brief yield between work batches
                    } else {
                        let idle_ms = if peer_best == 0 { 75 } else { 10 };
                        if tick_t0.elapsed() >= Duration::from_millis(500) {
                            crate::tlog!("[sync] ⚠ SLOW TICK: {} ms (backfill-branch) — sync loop was blocked this long, nothing else could progress", tick_t0.elapsed().as_millis());
                        }
                        tokio::time::sleep(Duration::from_millis(idle_ms)).await;
                    }
                }
            });
        });

        P2PBlockSync { state: state_struct, new_blocks, stop_tx: Some(stop_tx), recent_only, rebase_pending }
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

/// LANE-3 measurement (grogu-sync-perf, 2026-08-18): the sigil skill's RULE 0 demands
/// a real-code, real-hardware number, not a claim. A live network measurement was
/// confounded this session by an unrelated store/fork-rejection stall (separately
/// being fixed by grogu-sigil-sync in fetch.rs/verify.rs). This bench exercises the
/// EXACT production function the drain loop calls (`verify::decode_verify_backfill`,
/// unmodified) on realistic-size wire pages (mature headers carry a StarkProof + VDF
/// proof + fluxc bundle, ~8 KB/header per zstd_decompress_body's own comment — this
/// bench pads to match, not a stripped-down stub header), comparing the OLD shape
/// (decode pages one at a time, sequentially — what the drain loop did before this
/// session's change) against the NEW shape (decode the same batch via rayon par_iter
/// — what it does now) on THIS machine's real CPU. Not a live blk/s number; it IS
/// the real decode primitive, real header size, real hardware — the honest substitute
/// while the live rig is confounded.
#[cfg(test)]
mod lane3_decode_batch_bench {
    use super::verify::decode_verify_backfill;
    use sigil_header::*;
    use std::time::Instant;

    fn mk_header(height: u64, parent_hash: BlockHash, pad: usize) -> SigilBlockHeaderV0 {
        let nonce = SqiSignature::from_array([7u8; SQISIGN_L5_LEN]);
        let mut hh = blake3::Hasher::new();
        hh.update(&parent_hash);
        hh.update(nonce.as_bytes());
        let vdf_input: [u8; 32] = *hh.finalize().as_bytes();
        let scheme = SigScheme::SqiSign5;
        SigilBlockHeaderV0 {
            version: HEADER_VERSION,
            network_id: NETWORK_ID,
            height,
            parent_hash,
            merge_parents: Vec::new(),
            timestamp_ms: 1_000 + height,
            nonce_sqisign: nonce,
            vdf_input,
            // Realistic-size padding on the STARK proof bytes — this is the field the
            // decode.rs doc comment identifies as the dominant contributor to real
            // ~8 KB/header wire size (high-entropy proof bytes, not repetitive framing).
            vdf_proof: WesolowskiProof { y: vec![0u8; 256], pi: vec![0u8; 256], t: 100 },
            difficulty: 1,
            wallet_state_root: [0u8; 32],
            dex_state_root: [0u8; 32],
            event_log_root: [0u8; 32],
            contract_state_root: [0u8; 32],
            state_transition_proof: StarkProof { bytes: vec![0xABu8; pad], public_inputs_hash: [0u8; 32] },
            txs_merkle_root: [0u8; 32],
            tx_count: 0,
            fluxc_artifact_proof: ProofBundle {
                artifact_blake3: [0u8; 32],
                sqisign_sig: vec![0u8; 292], // SQIsign L5
                sqisign_pubkey: vec![0u8; 129],
                settle_tx: None,
            },
            sig_scheme: scheme,
            producer: [0u8; 32],
            producer_sig: SignatureBytes(vec![0u8; scheme.expected_sig_len()]),
            topology_commitment: None,
        }
    }

    fn mk_pages(n_pages: usize, headers_per_page: usize, pad: usize) -> Vec<Vec<u8>> {
        let mut parent = [0u8; 32];
        let mut pages = Vec::with_capacity(n_pages);
        for p in 0..n_pages {
            let mut chunk = Vec::with_capacity(headers_per_page);
            for i in 0..headers_per_page {
                let h = (p * headers_per_page + i) as u64;
                let hdr = mk_header(h, parent, pad);
                parent = hdr.hash();
                chunk.push(hdr);
            }
            let mut frame = vec![b'H'];
            frame.extend(bincode::serialize(&chunk).unwrap());
            pages.push(frame);
        }
        pages
    }

    #[test]
    fn parallel_batch_decode_matches_sequential_output() {
        // Correctness first: the batched path must produce byte-identical results to
        // decoding each page one at a time, just reordered by rayon then reassembled.
        let pages = mk_pages(6, 200, 512);
        let sequential: Vec<Vec<SigilBlockHeaderV0>> =
            pages.iter().map(|b| decode_verify_backfill(b)).collect();
        let parallel: Vec<Vec<SigilBlockHeaderV0>> = {
            use rayon::prelude::*;
            pages.par_iter().map(|b| decode_verify_backfill(b)).collect()
        };
        assert_eq!(sequential, parallel, "batching must not change WHAT gets decoded, only WHEN");
    }

    #[test]
    fn measure_batch_decode_wall_time_realistic_headers() {
        // Realistic shape: mature-chain headers (~8 KB each via the padding above),
        // CHUNK-sized pages (200 headers/page ≈ the general sync chunk width), a batch
        // of 16 pages in flight (SIGIL_SYNC_INFLIGHT's raised floor, per the v0.9.6
        // "raise default SIGIL_SYNC_INFLIGHT floor 8 -> 16" commit) — i.e. what actually
        // lands in `drained` on ONE drain-loop tick once the fetch side keeps the window
        // full (WAN latency / a healthy fast peer with request-ahead, not this session's
        // stalled loopback rig).
        let pages = mk_pages(16, 200, 6_000); // ~6 KB stark bytes + ~0.5KB other fields ≈ realistic
        let byte_total: usize = pages.iter().map(|p| p.len()).sum();

        let t0 = Instant::now();
        let sequential: Vec<Vec<SigilBlockHeaderV0>> =
            pages.iter().map(|b| decode_verify_backfill(b)).collect();
        let seq_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let parallel: Vec<Vec<SigilBlockHeaderV0>> = {
            use rayon::prelude::*;
            pages.par_iter().map(|b| decode_verify_backfill(b)).collect()
        };
        let par_ms = t1.elapsed().as_secs_f64() * 1000.0;

        assert_eq!(sequential, parallel, "parallel batch must match sequential exactly");
        let total_headers: usize = parallel.iter().map(|p| p.len()).sum();
        let seq_blk_s = total_headers as f64 / (seq_ms / 1000.0).max(1e-6);
        let par_blk_s = total_headers as f64 / (par_ms / 1000.0).max(1e-6);
        eprintln!(
            "[LANE3-BENCH] {total_headers} headers, {byte_total} wire bytes, 16 pages: \
             sequential={seq_ms:.1}ms ({seq_blk_s:.0} blk/s)  parallel={par_ms:.1}ms ({par_blk_s:.0} blk/s)  \
             speedup={:.2}x",
            seq_ms / par_ms.max(0.001)
        );
        // Not a hard perf assertion (CPU-count-dependent, would make the suite flaky on a
        // 1-2 core CI box) — the eprintln above is the honest, reproducible measurement;
        // this assert only guards against a build that's silently NOT running in parallel
        // at all (e.g. a rayon threadpool misconfiguration) on any multi-core box.
        if std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) >= 4 {
            assert!(par_ms < seq_ms, "batched decode should not be SLOWER than sequential on a multi-core box (seq={seq_ms:.1}ms par={par_ms:.1}ms)");
        }
    }
}
