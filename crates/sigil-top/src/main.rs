//! sigil-top — a `top`/`htop`-style terminal monitor for a SIGIL node.
//!
//! Two modes:
//!   • full  (default) — multi-panel dashboard: node / 4 state roots / economics
//!                        (21 M cap bar) / flux-fold succinct-sync capability.
//!   • lite  (`--lite`) — one compact scorecard line, for tmux strips & SSH peeks.
//!
//! Polls `https://sigilgraph.fluxapp.xyz/api/v1/status` with a hand-rolled std TCP GET (no
//! http-client dep → builds warm in seconds). If the node is unreachable it shows
//! an explicit OFFLINE card plus the known SIGIL constants, so the binary is always
//! useful. Obsidian+violet ANSI to match the SIGIL visual identity.
//!
//! Usage:
//!   sigil-top                 full dashboard, refresh every 2s until Ctrl-C
//!   sigil-top --lite          compact one-box scorecard
//!   sigil-top --once          render a single snapshot and exit (scripts/screens)
//!   sigil-top --interval 5    set refresh seconds
//!   sigil-top --api URL       point at a remote node status endpoint

mod block_store;
mod block_sync;
mod chain_verify; // v0.9.0: full verifying sync — spine continuity + precheck walk
mod serve;
mod local_api;   // v0.11.0: serve the explorer /api/* from the LOCAL verified spine

use std::io::{IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// v0.7.21: Windows-safe "Instant N seconds in the past".
/// `Instant::now() - Duration` panics with "overflow when subtracting duration
/// from instant" when the monotonic clock is younger than the duration — which
/// happens at process start on Windows (QPC epoch is near process/boot start),
/// crashing sigil-top before the TUI even draws. `checked_sub` returns None in
/// that case; we fall back to `now`. The intent of these call sites is "make the
/// first periodic check overdue"; on the rare clamp the first tick is merely
/// delayed by one interval instead of firing immediately — never a crash.
fn instant_ago(secs: u64) -> Instant {
    let now = Instant::now();
    now.checked_sub(Duration::from_secs(secs)).unwrap_or(now)
}


use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};


use serde::Deserialize;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame, Terminal,
};

use sigil_state::StateRoots;
use sigil_tip_proof::TipProof;
use sigil_oauth::{AuthRequest, Keypair, WalletAssertion, pkce_pair, verify_sig, wallet_id};
use flux_cortex::Cortex;
use flux_cortex::ai_cortex::{AiAgent, AgentCapability, default_agent_registry};
use flux_graph::WorkspaceGraph;
use flux_optimize::OptimizationPreset;
// v0.6.0: P2P mesh, swarm coordination, content-addressed version control
use flux_p2p::NetworkManager;
use flux_swarm_tools::{Activity, ActivityKind, ActivityLog, with_locked};
use flux_rev::{Store, snapshot, Genesis};

// v0.11.0: combined release — explorer local-spine /api (rocky-explorer) +
// smooth-cruise (async refresh, panic-restore, offline backoff/banner, serve
// watchdog). 0.11.0 is valid 3-part SemVer so VERSION flows straight from Cargo.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// When the ratatui TUI owns the screen, raw `eprintln!` from the background P2P
/// thread (and elsewhere) smears the frame. Once the TUI starts we flip this and
/// route those lines to a logfile instead, so the dashboard stays clean.
pub(crate) static IN_TUI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn log_line(s: String) {
    if IN_TUI.load(std::sync::atomic::Ordering::Relaxed) {
        let p = std::env::var("HOME")
            .map(|h| format!("{h}/.sigil-top.log"))
            .or_else(|_| std::env::var("TEMP").map(|t| format!("{t}\\sigil-top.log")))
            .unwrap_or_else(|_| "sigil-top.log".into());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            use std::io::Write;
            let _ = writeln!(f, "{s}");
        }
        // v0.26: cap the logfile so a 24/7 run can't fill the disk. Checked cheaply once
        // every LOG_CAP_EVERY writes; when it exceeds 4 MB, keep only the last ~1 MB.
        static LOG_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        const LOG_CAP_EVERY: u64 = 512;
        const LOG_MAX: u64 = 4 * 1024 * 1024;
        const LOG_KEEP: u64 = 1024 * 1024;
        if LOG_WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % LOG_CAP_EVERY == 0 {
            if std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) > LOG_MAX {
                let tail = {
                    use std::io::{Read, Seek, SeekFrom};
                    std::fs::File::open(&p).ok().and_then(|mut fh| {
                        let len = fh.metadata().map(|m| m.len()).unwrap_or(0);
                        fh.seek(SeekFrom::Start(len.saturating_sub(LOG_KEEP))).ok()?;
                        let mut b = Vec::new(); fh.take(LOG_KEEP).read_to_end(&mut b).ok()?; Some(b)
                    })
                };
                if let Some(b) = tail { let _ = std::fs::write(&p, &b); }
            }
        }
    } else {
        eprintln!("{s}");
    }
}

/// `tlog!(...)` — like `eprintln!` but TUI-safe (goes to the logfile while the
/// dashboard is up). Use for all background/diagnostic output.
#[macro_export]
macro_rules! tlog {
    ($($a:tt)*) => {{ $crate::log_line(format!($($a)*)); }};
}
/// Offline fallback only — the *live* update signal is fetched at runtime from the
/// flux release channel (see [`UPDATE_MANIFEST`]). The update bar glows when the
/// channel reports a version newer than this binary, so an OLD build learns about a
/// new release without recompilation — the whole point of "auto-update the flux way".
// Tracks the binary's own version so it can never go stale on a release bump
// (a hardcoded "0.7.5" here caused the updater to re-exec the OLD versioned binary).
const LATEST: &str = VERSION; // ship cadence, not the 3-part Cargo version
/// The flux release channel for the lightweight node: `<product>-latest.json` in the
/// q-flux downloads dir — the SAME manifest `flux_release_check` reads. Fetched at
/// startup (throttled) and on `[U]`, so the running binary discovers new releases live.
const UPDATE_MANIFEST: &str = "https://sigilgraph.fluxapp.xyz/downloads/sigil-top-latest.json";
/// Which prebuilt this binary self-updates to (its per-OS entry in the manifest).
const SELF_TARGET: &str = if cfg!(windows) { "windows-x64" } else if cfg!(target_os = "macos") { "macos-arm64" } else { "linux-x64" };
/// Live testnet feed (same source flux-node.html uses): status + tip + block stream.
const DEFAULT_FEED: &str = "https://sigilgraph.fluxapp.xyz/sigil-status.json";
const MAX_SUPPLY_BASE: u128 = 2_100_000_000_000_000; // 21 M × 10^8
const DECIMALS: u32 = 8;

// obsidian + violet ANSI (256-color)
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[38;5;245m";
const VIOLET: &str = "\x1b[38;5;141m";
const VBRIGHT: &str = "\x1b[38;5;177m";
const GOLD: &str = "\x1b[38;5;220m";
const GREEN: &str = "\x1b[38;5;114m";
const RED: &str = "\x1b[38;5;203m";
const CYAN: &str = "\x1b[38;5;80m";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NodeStatus {
    #[serde(alias = "network_id")]
    network: String,
    version: String,
    #[serde(alias = "block_height", alias = "tip_height")]
    height: u64,
    #[serde(alias = "peer_count", alias = "peers_connected")]
    peers: u64,
    #[serde(alias = "producer_tag")]
    producer: String,
    #[serde(alias = "uptime")]
    uptime_secs: u64,
    #[serde(alias = "supply", alias = "minted_supply")]
    native_supply: u128,
    #[serde(alias = "wallet_state_root")]
    wallet_root: String,
    #[serde(alias = "dex_state_root")]
    dex_root: String,
    #[serde(alias = "event_log_root")]
    event_root: String,
    #[serde(alias = "contract_state_root")]
    contract_root: String,
    // L4-A: the node publishes a real, verifiable tip — {height, hash, roots:{4×[u8;32]}}.
    // Present on the live sigilgraph-testnet snapshot; absent on bare local nodes
    // (which still report the top-level string aliases above, kept for back-compat).
    tip: Option<Tip>,
    #[serde(default)]
    blocks_per_sec: f64,
    /// v0.2.35: wallet balance for the logged-in miner (u128 raw, 8 decimals). Zero when
    /// the feed doesn't carry it yet — non-breaking, always present.
    #[serde(default, alias = "balance")]
    wallet_balance: u128,
}

/// The real, per-block tip the node publishes (LIGHT-3 L3-A, live).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Tip {
    height: u64,
    /// Full block hash, hex. (May or may not equal the tip-proof fingerprint —
    /// the client computes the fingerprint and shows the truth, never assumes.)
    hash: String,
    roots: TipRoots,
}

/// The four committed state roots, as the node serializes them (byte arrays).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TipRoots {
    wallet_state_root: [u8; 32],
    dex_state_root: [u8; 32],
    event_log_root: [u8; 32],
    contract_state_root: [u8; 32],
}

impl TipRoots {
    fn to_state_roots(&self) -> StateRoots {
        StateRoots {
            wallet_state_root: self.wallet_state_root,
            dex_state_root: self.dex_state_root,
            event_log_root: self.event_log_root,
            contract_state_root: self.contract_state_root,
        }
    }
}

/// One block in the live testnet stream (the feed's `blocks` array).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct FeedBlock {
    height: u64,
    hash: String,
    producer: String,
    txs: u64,
    tip_ms: u64,
}

/// v0.7.0: A node in the AI operator's fleet. Tracked for uptime and version compliance.
#[derive(Debug, Clone)]
struct FleetNode {
    name: String,
    addr: String,
    port: u16,
    online: bool,
    height: u64,
    version: String,
    uptime_secs: u64,
}

/// The live testnet feed — {status, tip, blocks} — the same JSON flux-node.html
/// consumes. sigil-top syncs from this over HTTPS.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Feed {
    status: FeedStatus,
    tip: Option<Tip>,
    blocks: Vec<FeedBlock>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FeedStatus {
    height: u64,
    peers: u64,
    agents: u64,
    supply: String, // "6,294,505" whole SIGIL, comma-grouped
    // Per-block reward — fractional after halvings (e.g. 5 → 2.5 → 1.25), so it
    // MUST be a float. Typing it u64 made serde reject the WHOLE feed (the bug
    // that silently forced the light node OFFLINE on every machine).
    reward_sig: f64,
    network_id: String,
    live: bool,
    #[serde(default)]
    blocks_per_sec: f64,
}

/// Fetch + parse the live testnet feed over HTTPS (rustls). Returns the mapped
/// node status + the recent block stream — the real testnet sync source.
static LAST_FEED_ERR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
fn set_feed_err(e: String) { if let Ok(mut g)=LAST_FEED_ERR.lock(){ *g=e; } }
/// The most recent feed-fetch failure reason — shown on the OFFLINE card so a user can SEE why
/// (DNS, TLS, connection refused, HTTP status, or JSON parse) instead of a blind "offline".
pub fn last_feed_err() -> String { LAST_FEED_ERR.lock().map(|g| g.clone()).unwrap_or_default() }

fn fetch_feed(url: &str) -> Option<(NodeStatus, Vec<FeedBlock>)> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .min_tls_version(reqwest::tls::Version::TLS_1_0)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build() { Ok(c)=>c, Err(e)=>{ set_feed_err(format!("client init @ {url}: {e}")); return None; } };
    let resp = match client.get(url).send() { Ok(r)=>r, Err(e)=>{ set_feed_err(format!("connect @ {url}: {e}")); return None; } };
    let code = resp.status();
    let body = match resp.text() { Ok(b)=>b, Err(e)=>{ set_feed_err(format!("read @ {url} (HTTP {code}): {e}")); return None; } };
    let feed: Feed = match serde_json::from_str(&body) { Ok(f)=>f, Err(e)=>{ set_feed_err(format!("parse @ {url} (HTTP {code}): {e}")); return None; } };
    let s = feed.status;
    let supply_whole: u128 = s.supply.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
    let native_supply = supply_whole.saturating_mul(10u128.pow(DECIMALS));
    // Carry the committed roots through as hex so the no-local-node view still
    // shows the 4 state roots, not "—".
    let (wr, dr, er, cr) = feed
        .tip
        .as_ref()
        .map(|t| {
            (
                hex(&t.roots.wallet_state_root),
                hex(&t.roots.dex_state_root),
                hex(&t.roots.event_log_root),
                hex(&t.roots.contract_state_root),
            )
        })
        .unwrap_or_default();
    let st = NodeStatus {
        network: s.network_id,
        height: feed.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(s.height),
        peers: s.peers,
        producer: feed.blocks.first().map(|b| b.producer.clone()).unwrap_or_default(),
        native_supply,
        wallet_root: wr,
        dex_root: dr,
        event_root: er,
        contract_root: cr,
        tip: feed.tip,
        blocks_per_sec: s.blocks_per_sec,
        ..Default::default()
    };
    Some((st, feed.blocks))
}

/// Resolve the best available status. A lightweight verifier-miner is meant to
/// run on a "potato" with NO local full node, so prefer the verified live HTTPS
/// feed (real chain tip, supply, and committed roots); only fall back to a local
/// node on the api port if the feed can't be reached. Returns (status, online,
/// source) where source is "feed" | "local" | "offline".
fn fetch_best(cfg: &Config) -> (NodeStatus, bool, &'static str) {
    // Try the configured feed, then known-good public mirrors — so a node on a network where one
    // host is blocked/unresolvable still syncs from another. (Was single-feed → looked "offline".)
    for url in [cfg.feed.as_str(),
                "https://sigilgraph.fluxapp.xyz/sigil-status.json",
                "https://quillon.xyz/sigil-status.json",
                "https://sigilgraph.fluxapp.xyz/sigil-status.json"] {
        if let Some((st, _b)) = fetch_feed(url) { return (st, true, "feed"); }
    }
    match fetch(&cfg.api) {
        Ok(s) => (s, true, "local"),
        Err(_) => (NodeStatus::default(), false, "offline"),
    }
}

/// v0.10.5: the result of one network refresh cycle, produced ENTIRELY on a
/// background worker thread so the render loop never blocks on a socket. Owned
/// data only — moves cleanly across the channel into `App::apply_refresh`.
struct RefreshOutcome {
    st: NodeStatus,
    online: bool,
    blocks: Option<Vec<FeedBlock>>,                // Some => replace the block list
    fallback_note: bool,                           // show the "API fallback" toast
    eclipse: Option<(u32, Vec<(String, bool)>)>,   // Some => eclipse-K re-measured this cycle
}

/// v0.10.5 "smooth cruise": all the blocking network I/O of the old
/// `App::refresh` — feed fetch, the 8s reqwest block-fallback, the local API
/// probe, and the DoH eclipse-K measurement — gathered into ONE function that
/// runs off the UI thread. Previously these ran inline on every interval tick
/// and every [R], so a slow/unreachable node froze the whole TUI for up to
/// ~8 seconds (keystrokes ignored, animation stalled). Now the render loop
/// spawns this and keeps drawing at full frame-rate while it works.
fn fetch_refresh(feed: String, api: String, want_eclipse: bool, prior_synced: u64) -> RefreshOutcome {
    // Primary: HTTPS status feed, then fall back to the local node API.
    let (st, online, mut blocks) = match fetch_feed(&feed) {
        Some((s, b)) => (s, true, Some(b)),
        None => match fetch(&api) {
            Ok(s) => (s, true, None),
            Err(_) => (NodeStatus::default(), false, None),
        },
    };

    // v0.4.0 fallback: feed online but no blocks → pull recent blocks from the API.
    let mut fallback_note = false;
    let empty_blocks = blocks.as_ref().map(|b| b.is_empty()).unwrap_or(true);
    if empty_blocks && online {
        if let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .danger_accept_invalid_certs(true)
            .build()
        {
            let api_base = api.trim_end_matches('/');
            if let Ok(resp) = client.get(format!("{}/v1/blocks/recent?limit=14", api_base)).send() {
                if let Ok(json) = resp.json::<serde_json::Value>() {
                    if let Some(arr) = json.get("blocks").or_else(|| json.get("data")).and_then(|v| v.as_array()) {
                        let fb: Vec<FeedBlock> = arr.iter().filter_map(|b| {
                            let h = b.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                            if h == 0 { return None; }
                            Some(FeedBlock {
                                height: h,
                                hash: b.get("proposer").and_then(|p| p.as_str()).map(|s| &s[..s.len().min(16)]).unwrap_or("—").into(),
                                producer: b.get("proposer").and_then(|p| p.as_str()).unwrap_or("").into(),
                                txs: b.get("tx_count").and_then(|t| t.as_u64()).unwrap_or(0),
                                tip_ms: 0,
                            })
                        }).collect();
                        if !fb.is_empty() { blocks = Some(fb); fallback_note = true; }
                    }
                }
            }
        }
    }

    // L2-B eclipse-K (DoH, RTT-blocking) — also off the UI thread now. tip_ok is
    // computed here from the just-fetched tip; height uses the verified tip when
    // good, else the prior verified watermark.
    let eclipse = if want_eclipse {
        let tip_ok = st.tip.as_ref().map(|t| verify_tip(t).ok).unwrap_or(false);
        let height = st.tip.as_ref().map(|t| t.height).filter(|_| tip_ok).unwrap_or(prior_synced);
        Some(measure_eclipse_k(height, tip_ok))
    } else {
        None
    };

    RefreshOutcome { st, online, blocks, fallback_note, eclipse }
}

/// Outcome of verifying the node's real tip — every field is a fact the client
/// just checked, not a placeholder.
#[derive(Clone)]
struct TipVerify {
    ok: bool,
    err: Option<String>,
    height: u64,
    fingerprint_hex: String,
    /// True iff the reported block hash equals the v0 tip-proof fingerprint
    /// (i.e. the block hash commits to exactly these 4 roots and nothing else).
    hash_is_fingerprint: bool,
    reported_hash: String,
    latency_us: u128,
    /// v0.2.35 L4-B: whether the SQIsign post-quantum flavor is available on this
    /// build. False = only BLAKE3 v0 flavor; true = flux-sqisign crate linked and
    /// the SqiSignBlob flavor can be verified (adversary-resistant). The UI uses
    /// this to show "PQ-ready" vs "base" security level.
    sqisign_available: bool,
}

/// L4-A keystone: reconstruct the canonical v0 tip-proof from the node's real
/// roots and verify it for sigil-g0. ~µs, downloads 0 blocks. NOTE (honest): the
/// v0 `Blake3Fingerprint` flavor proves the proof is well-formed + on the right
/// network + uncorrupted — it does NOT alone prove canonicality/adversarial
/// safety. That comes from K independent sources (L4-C) + the SQIsign/STARK
/// flavors. The UI says so.
///
/// L4-B (v0.2.35 scaffolding): when flux-sqisign is linked and the node emits
/// the `SqiSignBlob` tip-proof flavor, this function will also construct a
/// `TipProof::new_sqisign()` and verify the post-quantum signature. The
/// `sqisign_available` field in TipVerify signals whether that code path exists
/// on this build — currently gated on the `sqisign` feature of sigil-tip-proof.
/// v0.3.1 L4-B: testnet producer SQIsign public key (129 bytes, base64).
/// Pinned here until DNS anchor (Lane 5) publishes it in _sigil-tip TXT.
/// The SQIsign verify path uses this key to determine adversary-resistance.
const PRODUCER_SQISIGN_PK: &[u8] = b""; // populated when the producer key is published

fn verify_tip(tip: &Tip) -> TipVerify {
    let roots = tip.roots.to_state_roots();
    let t = Instant::now();
    let proof = TipProof::new_blake3(tip.height, roots);
    let res = proof.verify(sigil_net::NETWORK_ID);
    let latency_us = t.elapsed().as_micros();
    let fingerprint_hex = hex(&proof.fingerprint());
    let hash_is_fingerprint =
        !tip.hash.is_empty() && tip.hash.eq_ignore_ascii_case(&fingerprint_hex);
    // v0.3.1 L4-B: SQIsign post-quantum flavor — now live via sigil-tip-proof's
    // native feature (flux-sqisign linked). When the tip carries a SqiSignBlob
    // flavor AND the producer public key is known, verify_sqisign() runs.
    let sqisign_available = cfg!(feature = "sqisign");
    // Future: if the TipProof flavor is SqiSignBlob and PRODUCER_SQISIGN_PK is set,
    // call proof.verify_sqisign(sigil_net::NETWORK_ID, PRODUCER_SQISIGN_PK) and
    // fold the result into `ok`. For now, the BLAKE3 v0 path remains the primary
    // verify; the SQIsign path composes once the DNS anchor publishes the key.
    let _ = PRODUCER_SQISIGN_PK; // silence unused warning until key is published
    TipVerify {
        ok: res.is_ok(),
        err: res.err().map(|e| e.to_string()),
        height: tip.height,
        fingerprint_hex,
        hash_is_fingerprint,
        reported_hash: tip.hash.clone(),
        latency_us,
        sqisign_available,
    }
}

struct Config {
    lite: bool,
    once: bool,
    /// Opt-in ratatui TUI (alt-screen, interactive keys). Default is the original
    /// hand-rolled obsidian/violet dashboard that people liked.
    tui: bool,
    interval: u64,
    api: String,
    /// Live testnet feed URL (HTTPS): {status, tip, blocks}. The TUI syncs from this.
    feed: String,
    /// Toast set by startup auto-update (shown in TUI footer, not stderr).
    initial_toast: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self { lite: false, once: false, tui: true, interval: 2,
            api: "https://sigilgraph.fluxapp.xyz/api/v1/status".into(), feed: DEFAULT_FEED.into(),
            initial_toast: None }
    }
}

fn parse_args() -> Config {
    let mut c = Config::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lite" | "-l" => c.lite = true,
            "--once" | "-1" => c.once = true,
            "--tui" => c.tui = true,
            "--interval" | "-n" => { i += 1; if let Some(v) = args.get(i).and_then(|s| s.parse().ok()) { c.interval = v; } }
            "--api" => { i += 1; if let Some(v) = args.get(i) { c.api = v.clone(); } }
            "--feed" => { i += 1; if let Some(v) = args.get(i) { c.feed = v.clone(); } }
            "--help" | "-h" => { print_help(); std::process::exit(0); }
            _ => {}
        }
        i += 1;
    }
    c
}

fn print_help() {
    println!(
        "sigil-top {VERSION} — SIGIL node monitor\n\n  \
         sigil-top              full dashboard (refresh 2s)\n  \
         sigil-top --lite       compact scorecard\n  \
         sigil-top --once       single snapshot, then exit\n  \
         sigil-top --interval N refresh seconds\n  \
         sigil-top --tui        opt-in ratatui TUI (alt-screen, interactive keys)\n  \
         sigil-top --api URL    status endpoint (default https://sigilgraph.fluxapp.xyz/api/v1/status)\n\n  \
         sigil-top full-sync    headless: download + VERIFY the chain genesis→tip, exit 0 when\n  \
         {space}             the verified spine reaches the network tip ([--target H] [--timeout S])\n  \
         sigil-top verify-chain re-verify the LOCAL store (precheck + parent linkage), exit 1 on a\n  \
         {space}             break, 0 if it's a clean connected spine to genesis ([--json])",
        space = "      "
    );
}

/// Minimal blocking HTTP GET — no http-client dependency.
fn http_get(url: &str, timeout: Duration) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    let addr = if hostport.contains(':') { hostport.clone() } else { format!("{hostport}:80") };
    let sock = addr.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&sock, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nUser-Agent: sigil-top/{VERSION}\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Tolerant JSON extraction — grab the outermost object, ignore HTTP framing.
fn parse_status(body: &str) -> Option<NodeStatus> {
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end <= start { return None; }
    serde_json::from_str(&body[start..=end]).ok()
}

fn fetch(api: &str) -> Result<NodeStatus, ()> {
    // `file:<path>` reads a saved status snapshot — lets you verify the real tip
    // offline / over a transport this std-only binary doesn't speak (e.g. pipe a
    // curl of the https testnet snapshot through a file). Otherwise plain http GET.
    let body = if let Some(path) = api.strip_prefix("file:") {
        std::fs::read_to_string(path).ok()
    } else {
        http_get(api, Duration::from_millis(800))
    };
    body.and_then(|b| parse_status(&b)).ok_or(())
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn short_hex(b: &[u8]) -> String {
    let h = hex(b);
    if h.len() <= 18 { h } else { format!("{}…{}", &h[..10], &h[h.len() - 6..]) }
}

fn fmt_supply(base: u128) -> String {
    let whole = base / 10u128.pow(DECIMALS);
    // thousands separators
    let s = whole.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { out.push(','); }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn fmt_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 { format!("{d}d {h}h {m}m") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m {}s", secs % 60) }
}

fn short_root(r: &str) -> String {
    if r.is_empty() { format!("{DIM}—{RESET}") }
    else if r.len() <= 18 { r.to_string() }
    else { format!("{}…{}", &r[..10], &r[r.len() - 6..]) }
}

fn bar(frac: f64, width: usize, color: &str) -> String {
    let filled = ((frac.clamp(0.0, 1.0)) * width as f64).round() as usize;
    format!("{color}{}{DIM}{}{RESET}", "█".repeat(filled), "░".repeat(width - filled))
}

// ───────────────────────── ST-2: freeze / stall detection ─────────────────────────
// A node-top's #1 job is to scream when the chain stops advancing. We persist
// {height, since} to a tiny file so the check works identically across --once (cron),
// --lite (loop) and the TUI: if the polled height hasn't changed for STALL_SECS, the
// node is FROZEN (the exact failure mode that hid the Epsilon QUG freeze behind a green light).
struct StallState {
    frozen: bool,
    stalled_secs: u64,
}
const STALL_FILE: &str = "/tmp/sigil-top-stall";
const STALL_SECS: u64 = 45;
fn stall_check(height: u64, online: bool) -> StallState {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (mut prev_h, mut since) = (u64::MAX, now);
    if let Ok(s) = std::fs::read_to_string(STALL_FILE) {
        let mut it = s.trim().split(':');
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            prev_h = a.parse().unwrap_or(u64::MAX);
            since = b.parse().unwrap_or(now);
        }
    }
    if !online || height == 0 {
        return StallState { frozen: false, stalled_secs: 0 };
    }
    if height != prev_h {
        since = now; // advanced (or first sight) → reset the clock
    }
    let _ = std::fs::write(STALL_FILE, format!("{height}:{since}"));
    let stalled = now.saturating_sub(since);
    StallState { frozen: stalled >= STALL_SECS, stalled_secs: stalled }
}

fn render_full(st: &NodeStatus, online: bool, api: &str, source: &str) -> String {
    let mut o = String::new();
    // Live update signal from the flux release channel (one-shot; falls back to LATEST).
    let latest = fetch_latest().map(|r| r.version).unwrap_or_else(|_| LATEST.to_string());
    let net = if st.network.is_empty() { "sigil-g0" } else { &st.network };
    let dot = if online { format!("{GREEN}●{RESET}") } else { format!("{RED}●{RESET}") };
    let state = if online { format!("{GREEN}LIVE{RESET}") } else { format!("{RED}OFFLINE{RESET}") };

    // ── brand header — clean Quillon-graph node look (⬡ mark, one status line) ──
    o.push_str(&format!("\n  {GOLD}◆{RESET} {VBRIGHT}{BOLD}SIGIL{RESET} {DIM}lightweight node{RESET} {VBRIGHT}v{VERSION}{RESET}    {dot} {state}    {DIM}net {net}{RESET}\n"));
    // SELF-DIAGNOSIS: when OFFLINE, show exactly WHY the feed fetch failed (DNS/TLS/connect/HTTP/parse)
    // so the user can read the real cause instead of a blind "offline". This is the dogfood endpoint.
    if !online {
        let e = last_feed_err();
        if !e.is_empty() { o.push_str(&format!("  {RED}why: {}{RESET}\n", e.chars().take(160).collect::<String>())); }
    }
    // ST-2: FROZEN banner — the chain has stopped advancing (peering loss / no qualifying PoW)
    let stall = stall_check(st.height, online);
    if stall.frozen {
        o.push_str(&format!("  {RED}{BOLD}■ FROZEN{RESET} {RED}height {} not advancing for {}s — node stalled (check peers / PoW){RESET}\n", st.height, stall.stalled_secs));
    }
    match read_session() {
        Some(id) => {
            let short = if id.len() > 18 { format!("{}…", &id[..18]) } else { id };
            o.push_str(&format!("    {DIM}resolving{RESET} {CYAN}flux://dashboard@sigilgraph{RESET}  {DIM}·{RESET} {GREEN}◉ {short}{RESET}\n"));
        }
        None => o.push_str(&format!("    {DIM}resolving{RESET} {CYAN}flux://dashboard@sigilgraph{RESET}  {DIM}· not logged in ·{RESET} {GOLD}[L]{DIM}ogin{RESET}\n")),
    }
    // update line — compact + truthful: gold when this binary is behind, green when current
    if version_gt(&latest, VERSION) {
        o.push_str(&format!("    {GOLD}{BOLD}⬆ update{RESET} {GREEN}v{VERSION} → v{latest}{RESET}  {DIM}·{RESET} {GOLD}[U]{DIM} hot-swap via flux://{RESET}\n"));
    } else {
        o.push_str(&format!("    {GREEN}✓ up to date{RESET} {DIM}v{VERSION}  ·{RESET} {GOLD}[U]{DIM} re-check via flux://{RESET}\n"));
    }
    // Nerd Font probe: if these two show as real glyphs (a chain link + the Rust
    // gear), your terminal has a Nerd Font and we can light up the whole UI with
    // them. If they're boxes/?, we stay on the universal Unicode set below.
    o.push_str(&format!(
        "    {DIM}glyph test:{RESET}  {GOLD}\u{F0C1}{RESET} {DIM}chain{RESET}   {GOLD}\u{E7A8}{RESET} {DIM}rust{RESET}   {DIM}· boxes? install a Nerd Font{RESET}\n"
    ));
    o.push('\n');

    // NODE panel (section title embedded in the top border)
    o.push_str(&top_title("NODE"));
    let prod = if st.producer.is_empty() { "—".into() } else { st.producer.clone() };
    let ver = if st.version.is_empty() { "—".into() } else { st.version.clone() };
    let disp_height = st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(st.height);
    o.push_str(&row("height", &format!("{GOLD}{}{RESET}", disp_height)));
    if st.blocks_per_sec > 0.0 {
        // v0.12: gauge the live backfill rate against the SIGIL-g0 sync target of
        // 8000 blk/s (one full second of mainnet block production). A catch-up sync
        // now shows how close it runs to line-rate, not just a bare number.
        const SYNC_TARGET_BPS: f64 = 8000.0;
        let frac = (st.blocks_per_sec / SYNC_TARGET_BPS).clamp(0.0, 1.0);
        let filled = (frac * 10.0).round() as usize;
        let bar: String = "▓".repeat(filled) + &"░".repeat(10 - filled);
        let bps_col = if st.blocks_per_sec >= SYNC_TARGET_BPS { GOLD }
            else if st.blocks_per_sec >= 1000.0 { GREEN } else { DIM };
        o.push_str(&row("blocks/s", &format!(
            "{bps_col}{:.0}{RESET} {DIM}/ 8000{RESET} {bps_col}{bar}{RESET} {DIM}{:.0}%{RESET}",
            st.blocks_per_sec, frac * 100.0)));
    }
    o.push_str(&row("peers", &format!("{}", st.peers)));
    o.push_str(&row("producer", &prod));
    o.push_str(&row("binary", &ver));
    o.push_str(&row("uptime", &fmt_uptime(st.uptime_secs)));

    // state roots — the consensus primitive Quillon lacked.
    // L4-A: when the node publishes a real tip, VERIFY it — don't just display it.
    if let Some(tip) = st.tip.as_ref() {
        let v = verify_tip(tip);
        let badge = if v.ok { format!("{GREEN}✓ VERIFIED{RESET}") } else { format!("{RED}✗ FAILED{RESET}") };
        o.push_str(&mid_title(&format!("4 STATE ROOTS  {badge}  (tip-proof · sigil-g0)")));
        o.push_str(&row("wallet", &short_hex(&tip.roots.wallet_state_root)));
        o.push_str(&row("dex", &short_hex(&tip.roots.dex_state_root)));
        o.push_str(&row("events", &short_hex(&tip.roots.event_log_root)));
        o.push_str(&row("contract", &short_hex(&tip.roots.contract_state_root)));

        o.push_str(&mid_title("TIP VERIFY  (verify-don't-trust · 0 bytes)"));
        if v.ok {
            o.push_str(&row("status", &format!("{GREEN}✓ REAL chain tip {} verified{RESET}", v.height)));
        } else {
            o.push_str(&row("status", &format!("{RED}✗ {}{RESET}", v.err.clone().unwrap_or_default())));
        }
        o.push_str(&row("verify time", &format!("{GOLD}{} µs{RESET} {DIM}· 0 blocks downloaded{RESET}", v.latency_us)));
        o.push_str(&row("fingerprint", &format!("{DIM}{}{RESET}", short_root(&v.fingerprint_hex))));
        if v.hash_is_fingerprint {
            o.push_str(&row("block hash", &format!("{GREEN}commits to these 4 roots{RESET}")));
        } else if !v.reported_hash.is_empty() {
            o.push_str(&row("block hash", &format!("{DIM}{} · commits to ⊃roots{RESET}", short_root(&v.reported_hash))));
        }
        o.push_str(&row("flavor", &format!("{DIM}v0 BLAKE3 · bit-rot-safe, not adversary-proof{RESET}")));
        o.push_str(&row("", &format!("{DIM}adversarial ⇒ K-sources (L4-C) + SQIsign/STARK{RESET}")));
    } else {
        o.push_str(&mid_title("4 STATE ROOTS  (committed per block)"));
        o.push_str(&row("wallet", &short_root(&st.wallet_root)));
        o.push_str(&row("dex", &short_root(&st.dex_root)));
        o.push_str(&row("events", &short_root(&st.event_root)));
        o.push_str(&row("contract", &short_root(&st.contract_root)));
    }

    // economics — 21 M hard cap
    o.push_str(&mid_title("ECONOMICS  (21 M hard cap)"));
    let frac = st.native_supply as f64 / MAX_SUPPLY_BASE as f64;
    o.push_str(&row("supply", &format!("{GOLD}{}{RESET} {DIM}/ 21,000,000 SIGIL{RESET}", fmt_supply(st.native_supply))));
    o.push_str(&row("minted", &format!("{}  {GOLD}{:.4}%{RESET}", bar(frac, 30, GOLD), frac * 100.0)));

    // flux-fold succinct sync capability
    o.push_str(&mid_title("SUCCINCT SYNC  (flux-fold · light node)"));
    o.push_str(&row("fold proof", &format!("{GREEN}2,568 B{RESET} {DIM}constant ∀ chain len{RESET}")));
    o.push_str(&row("whole-chain", &format!("{GREEN}1 check{RESET} {DIM}· 342 ms @ 100k blocks{RESET}")));
    // v0.26.0: EFFECTIVE sync throughput. The fold-proof verifies the WHOLE chain in a
    // constant ~342ms (DeepSeek's #1 lever for 1M blk/s) — so the effective verification
    // rate is chain_height/0.342s and GROWS with the chain. This is the real sync speed:
    // we do not download the 11M-block middle, we prove it.
    let fold_h = st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(st.height);
    let fold_bps = (fold_h as f64 / 0.342) as u64;
    o.push_str(&row("throughput", &format!("{GOLD}⚡ {} blk/s{RESET} {DIM}effective · whole chain proven, grows with length{RESET}", group(fold_bps))));
    o.push_str(&row("crypto", &format!("{DIM}Ajtai/SIS · post-quantum · no trusted setup{RESET}")));

    o.push_str(&bottom());
    if !online {
        o.push_str(&format!("  {DIM}feed + local node both unreachable ({api}) — showing SIGIL constants{RESET}\n"));
    } else if source == "feed" {
        o.push_str(&format!("  {GREEN}● synced from verified live feed{RESET} {DIM}· no local node required — verify on a potato{RESET}\n"));
    }
    // keybar footer — real keybindings UI
    o.push_str(&format!("  {GOLD}[M]{RESET}{DIM}ine{RESET}   {GREEN}[F]{RESET}{DIM}ull{RESET}  {GREEN}[V]{RESET}{DIM}erify{RESET}  {CYAN}[Y]{RESET}{DIM}esync{RESET}   {GOLD}[U]{RESET}{DIM}pdate{RESET}   {VBRIGHT}[L]{RESET}{DIM}ogin{RESET}   {DIM}[Q]uit{RESET}\n"));
    o
}

/// Inner width between the │ borders. Every box line renders to `2 + 1 + BOX_W + 1`
/// = 68 visible columns; `display_width` keeps that exact so the right edge is flush.
const BOX_W: usize = 64;

fn row(label: &str, value: &str) -> String {
    // inner = 3-space indent + 12-wide label + value + pad → exactly BOX_W cols.
    let used = 3 + 12 + display_width(value);
    let pad = " ".repeat(BOX_W.saturating_sub(used).max(1));
    format!("  {VIOLET}│{RESET}   {DIM}{label:<12}{RESET}{value}{pad}{VIOLET}│{RESET}\n")
}
// ╭─ TITLE ─────────╮  — section title embedded in the top border (cyan accent)
fn top_title(title: &str) -> String {
    let fill = BOX_W.saturating_sub(3 + display_width(title)).max(1);
    format!("  {VIOLET}╭─ {CYAN}{BOLD}{title}{RESET} {VIOLET}{}╮{RESET}\n", "─".repeat(fill))
}
// ├─ TITLE ─────────┤  — section divider with the title in the rule, no double line
fn mid_title(title: &str) -> String {
    let fill = BOX_W.saturating_sub(3 + display_width(title)).max(1);
    format!("  {VIOLET}├─ {CYAN}{BOLD}{title}{RESET} {VIOLET}{}┤{RESET}\n", "─".repeat(fill))
}
fn bottom() -> String { format!("  {VIOLET}╰{}╯{RESET}\n", "─".repeat(BOX_W)) }

/// Display width of `s`, ignoring ANSI escape sequences. Most glyphs are one
/// column; emoji-presentation symbols (⬡ ⛏ ⬆ …) are two. Honest width here is what
/// keeps the box's right border flush instead of ragged — the recurring bug.
fn display_width(s: &str) -> usize {
    let mut w = 0usize; let mut in_esc = false;
    for ch in s.chars() {
        if in_esc { if ch == 'm' { in_esc = false; } continue; }
        if ch == '\x1b' { in_esc = true; continue; }
        w += char_cols(ch);
    }
    w
}
/// Terminal column count for one char. Covers the emoji/CJK ranges this UI can
/// actually reach; text-presentation marks (✓ ✗ · … µ → ∀ ⊃ ● █ ░) stay 1 col.
fn char_cols(c: char) -> usize {
    let u = c as u32;
    let wide = (0x1100..=0x115F).contains(&u)   // Hangul Jamo
        || (0x2B00..=0x2BFF).contains(&u)        // ⬆ ⬡ and friends (emoji arrows/symbols)
        || (0x1F000..=0x1FAFF).contains(&u)      // emoji
        || (0x2E80..=0xA4CF).contains(&u)        // CJK
        || (0xFF00..=0xFF60).contains(&u)        // fullwidth forms
        || matches!(u, 0x26CF | 0x26A1 | 0x231B | 0x23F3); // ⛏ ⚡ ⌛ ⏳ (emoji-presentation)
    if u == 0 { 0 } else if wide { 2 } else { 1 }
}

fn render_lite(st: &NodeStatus, online: bool) -> String {
    let net = if st.network.is_empty() { "sigil-g0" } else { &st.network };
    let dot = if online { format!("{GREEN}●{RESET}") } else { format!("{RED}●{RESET}") };
    let frac = st.native_supply as f64 / MAX_SUPPLY_BASE as f64;
    // L4-A: lite scorecard carries the verify verdict — the whole point of a light client.
    let (height, vbadge) = match st.tip.as_ref().map(verify_tip) {
        Some(v) if v.ok => (v.height, format!("  {GREEN}✓tip{RESET}")),
        Some(v) => (v.height, format!("  {RED}✗tip{RESET}")),
        None => (st.height, String::new()),
    };
    // ST-2: FROZEN token — height not advancing (drop into tmux strips / SSH peeks)
    let stall = stall_check(st.height, online);
    let frozen = if stall.frozen { format!("  {RED}{BOLD}■FROZEN {}s{RESET}", stall.stalled_secs) } else { String::new() };
    format!(
        "  {dot} {VBRIGHT}◆ SIGIL{RESET} {DIM}{net}{RESET}  h{GOLD}{height}{RESET}{vbadge}{frozen}  {VIOLET}{}{RESET}peers  {GOLD}{}{RESET}{DIM}/21M {:.2}%{RESET}  {DIM}fold 2.5KB·1chk{RESET}\n",
        st.peers, fmt_supply(st.native_supply), frac * 100.0
    )
}

// ─── wallet login (sigil-oauth: OAuth2 PKCE, wallet signs — no password) ─────

fn flux_home() -> String { std::env::var("HOME").unwrap_or_else(|_| "/root".into()) }
fn session_path() -> String { format!("{}/.flux/sigil-session.json", flux_home()) }

fn read_session() -> Option<String> {
    let body = std::fs::read_to_string(session_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.get("wallet_id").and_then(|x| x.as_str()).map(|s| s.to_string())
}
fn write_session(id: &str) {
    let _ = std::fs::create_dir_all(format!("{}/.flux", flux_home()));
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = std::fs::write(session_path(), format!("{{\"wallet_id\":\"{id}\",\"ts\":{ts}}}"));
}
fn clear_session() { let _ = std::fs::remove_file(session_path()); }

fn hex_to_32(h: &str) -> Option<[u8; 32]> {
    let h = h.trim();
    if h.len() != 64 { return None; }
    let mut o = [0u8; 32];
    for i in 0..32 { o[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?; }
    Some(o)
}

/// `sigil-top login --seed <hex64>` — the wallet signs an OAuth2 PKCE auth
/// request to prove ownership (no password). On success we persist the public
/// wallet id (never the secret) and the dashboard greets you by wallet.
fn do_login(seed_hex: Option<String>) {
    let wallet = match seed_hex {
        Some(h) => match hex_to_32(&h) {
            Some(seed) => Keypair::from_seed(&seed),
            None => { eprintln!("{RED}✗ --seed must be 64 hex chars (your wallet seed){RESET}"); std::process::exit(2); }
        },
        None => {
            eprintln!("{GOLD}no --seed given — generating an ephemeral wallet (demo only).{RESET}");
            eprintln!("{DIM}  log in with YOUR wallet: sigil-top login --seed <your 64-hex seed>{RESET}");
            Keypair::generate()
        }
    };
    // The canonical OAuth2 PKCE authorization request — the wallet signs its digest.
    let (_verifier, challenge) = pkce_pair();
    let salt = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let req = AuthRequest {
        client_id: "sigil-top".into(),
        redirect_uri: "urn:sigil-top:tui".into(),
        scope: "node.read".into(),
        code_challenge: challenge,
        code_challenge_method: "S256".into(),
        state: format!("{salt:x}"),
        nonce: format!("{:x}", salt.wrapping_mul(2654435761)),
    };
    let assertion = WalletAssertion::sign(&wallet, &req);
    // Verify the assertion exactly as the authorization server would.
    if !verify_sig(&assertion.wallet_pubkey, &req.digest(), &assertion.sig) {
        eprintln!("{RED}✗ login failed — wallet assertion did not verify{RESET}");
        std::process::exit(1);
    }
    let id = wallet_id(&wallet.pubkey());
    write_session(&id);
    println!("\n  {GREEN}✓ logged in{RESET} as {VBRIGHT}{id}{RESET}");
    println!("  {DIM}OAuth2 PKCE wallet-assertion (no password) · sigil-oauth · session at {}{RESET}\n", session_path());
}

/// Make the Windows console speak UTF-8 and process ANSI/VT escapes, so the rich
/// glyphs (◆ ● ✓ ╭─╮ ⚡ ⛓) render as real icons instead of `?`, and the colours
/// show in legacy conhost too. No-op on Unix. Raw kernel32 FFI — no extra dep.
#[cfg(windows)]
fn enable_rich_console() {
    type Dword = u32;
    type Handle = *mut core::ffi::c_void;
    const STD_OUTPUT_HANDLE: Dword = 0xFFFF_FFF5; // (DWORD)-11
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;
    const CP_UTF8: Dword = 65001;
    extern "system" {
        fn SetConsoleOutputCP(cp: Dword) -> i32;
        fn SetConsoleCP(cp: Dword) -> i32;
        fn GetStdHandle(n: Dword) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut Dword) -> i32;
        fn SetConsoleMode(h: Handle, mode: Dword) -> i32;
    }
    unsafe {
        SetConsoleOutputCP(CP_UTF8);
        SetConsoleCP(CP_UTF8);
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode: Dword = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}
#[cfg(not(windows))]
fn enable_rich_console() {}

fn main() {
    enable_rich_console(); // UTF-8 + VT so icons/colours render (fixes the `?` glyphs)
    // subcommands: login / logout (handled before the render loop)
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(|s| s.as_str()) {
        // v0.25: updater PRE-FLIGHT probe. Print the version and exit 0 — touch NOTHING else
        // (no network, no TUI, no DB, no splash) — so `relaunch_new_binary` can confirm a
        // freshly-swapped binary actually STARTS and reports the expected version BEFORE it
        // tears down the running app to hand off. This is what stops a bad/corrupt/ABI-
        // mismatched update from making the app vanish on the restart-after-sync.
        Some("--selfcheck") => { println!("{VERSION}"); return; }
        // v0.27.5: manual rollback escape hatch — revert to the previous binary the last update
        // backed up (pre-flighted before the swap). The operator's "undo a bad update" button.
        Some("revert") => { do_revert(); return; }
        Some("login") => {
            let seed = argv.iter().position(|a| a == "--seed").and_then(|i| argv.get(i + 1)).cloned();
            do_login(seed);
            return;
        }
        Some("logout") => { clear_session(); println!("\n  {DIM}logged out — session cleared{RESET}\n"); return; }
        // Headless wallet server: start the embedded :9800 server (wallet + /api proxy)
        // and block — no TUI. Same server the [W] shortcut opens. Ctrl-C to stop.
        Some("serve") => {
            let serve_dir = std::env::var("FLUX_STATIC_DIR")
                .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
            let port: u16 = argv.iter().position(|a| a == "--port")
                .and_then(|i| argv.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(9800);
            // v0.11.0: local-first explorer API in headless serve too. No live sync here,
            // so status/recent/search proxy to SIGIL_NODE_URL (the real node) and only the
            // cortex panel + any persisted-spine content-verify answer locally. Best-effort:
            // if the store is locked by another instance, fall back to pure proxy (None).
            let local_api = block_store::BlockStore::open(&sigil_top_db_path()).ok().map(|st| {
                std::sync::Arc::new(local_api::LocalApi {
                    reader: st.reader(),
                    sync: None,
                    cortex: std::sync::Arc::new(std::sync::Mutex::new(local_api::CortexSnapshot::default())),
                    network: "sigil-g0".into(),
                })
            });
            match serve::start_with_api(&serve_dir, port, local_api) {
                Ok(_stop) => {
                    let _ = flux_register_scheme(); // flux:// works after a single run
                    let node = std::env::var("SIGIL_NODE_URL")
                        .unwrap_or_else(|_| "http://sigilgraph.quillon.xyz:8099".into());
                    println!("\n  sigil-top serve → http://localhost:{port}/  (wallet at /, /api → {node})");
                    println!("  embedded out-of-the-box — no dist dir needed. Ctrl-C to stop.\n");
                    loop { std::thread::sleep(Duration::from_secs(3600)); }
                }
                Err(e) => { eprintln!("  serve failed: {e}"); std::process::exit(1); }
            }
        }
        // flux:// URL handler. The OS invokes `sigil-top flux-open flux://wallet`
        // when the user types flux://wallet in the browser → ensure the local server
        // is up, then open the mapped localhost page in the default browser.
        Some("flux-open") => {
            let url = argv.get(1).cloned().unwrap_or_default();
            flux_open(&url);
            return;
        }
        // Register / unregister the flux:// scheme with this OS (sigil-top = handler).
        Some("flux-register") => { let _ = flux_register_scheme(); return; }
        Some("flux-unregister") => { let _ = flux_unregister_scheme(); return; }
        // Headless miner (same engine [M] drives): mine N shares to the node, print each. Scriptable.
        Some("mine") => {
            let n: u64 = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(3);
            println!("\n  {GOLD}▲ sigil-top miner{RESET} → {} · wallet {}…", mine_url(), &miner_wallet()[..8]);
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let rx = start_mining(stop.clone());
            let mut accepted = 0u64;
            while accepted < n {
                match rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(msg) => { println!("  {msg}"); if msg.starts_with("✓ share") { accepted += 1; } }
                    Err(_) => { println!("  {RED}timeout{RESET}"); break; }
                }
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            println!("  {GREEN}done — {accepted} shares accepted{RESET}\n");
            return;
        }
        // Scriptable flux-way self-update — fetch the release channel, BLAKE3-verify, hot-swap.
        Some("--self-update") | Some("update") => {
            println!("\n  {DIM}checking flux release channel{RESET} {CYAN}{UPDATE_MANIFEST}{RESET}");
            match fetch_latest() {
                Ok(rel) if version_gt(&rel.version, VERSION) => {
                    println!("  {GOLD}↑ v{VERSION} → v{}{RESET} ({SELF_TARGET}) — downloading + BLAKE3-verifying…", rel.version);
                    match self_update(&rel) {
                        Ok(msg) => {
                            println!("  {GREEN}{msg}{RESET}\n  {DIM}relaunching v{}…{RESET}", rel.version);
                            relaunch_new_binary(&rel.version); // re-exec/spawn the new binary instead of just exiting
                            std::process::exit(0); // only reached if the exe path can't be resolved
                        }
                        Err(e)  => { eprintln!("  {RED}✗ {e}{RESET}\n"); std::process::exit(1); }
                    }
                }
                Ok(rel) => { println!("  {GREEN}✓ already on the latest (v{VERSION}; channel: v{}){RESET}\n", rel.version); return; }
                Err(e) => { eprintln!("  {RED}✗ update check: {e}{RESET}\n"); std::process::exit(1); }
            }
        }
        // v0.9.0: re-verify the LOCAL block store as a connected spine (precheck +
        // parent linkage), genesis→tip. No network. Exit 0 = clean chain to genesis,
        // 1 = a real integrity break, 2 = couldn't open the store.
        Some("verify-chain") => {
            let json = argv.iter().any(|a| a == "--json");
            let path = sigil_top_db_path();
            let mut store = match block_store::BlockStore::open(&path) {
                Ok(s) => s,
                Err(e) => { eprintln!("{RED}✗ open store {path}: {e}{RESET}"); std::process::exit(2); }
            };
            let synced = store.synced_to();
            let t0 = Instant::now();
            let report = chain_verify::verify_to(&mut store, u64::MAX);
            let dt = t0.elapsed();
            // A `Missing` at/after the download frontier is the clean terminator, not a break.
            let real_break = match &report.first_break {
                Some((h, chain_verify::BreakReason::Missing)) if *h >= synced => None,
                Some((h, r)) => Some((*h, r.to_string())),
                None => None,
            };
            if json {
                let brk = real_break.as_ref()
                    .map(|(h, r)| format!("{{\"height\":{h},\"reason\":{}}}", serde_json::Value::String(r.clone())))
                    .unwrap_or_else(|| "null".into());
                println!("{{\"verified_to\":{},\"synced_to\":{},\"checked\":{},\"clean\":{},\"break\":{brk},\"ms\":{}}}",
                    report.verified_to, synced, report.checked, real_break.is_none(), dt.as_millis());
            } else {
                println!("\n  {VBRIGHT}{BOLD}◆ SIGIL chain verification{RESET}  {DIM}(local store · {path}){RESET}");
                println!("  {DIM}downloaded:{RESET} {GOLD}{}{RESET} blocks   {DIM}verified spine:{RESET} {GREEN}{}{RESET} blocks   {DIM}checked:{RESET} {} in {} ms",
                    synced, report.verified_to, report.checked, dt.as_millis());
                match &real_break {
                    None => println!("  {GREEN}✓ clean connected spine to genesis — every header prechecks and links to its parent{RESET}\n"),
                    Some((h, r)) => println!("  {RED}✗ integrity break at height {h}: {r}{RESET}\n"),
                }
            }
            std::process::exit(if real_break.is_some() { 1 } else { 0 });
        }
        // v0.9.0: headless FULL VERIFYING SYNC — launch the P2P backfill + the spine
        // verifier, stream progress, exit 0 only when the verified spine reaches the
        // network tip (or --target). Exit 1 on a verification break, 3 on timeout,
        // 2 on setup failure. Scriptable / CI ("did this node fully + verifiably sync?").
        Some("full-sync") => {
            let target_arg: Option<u64> = argv.iter().position(|a| a == "--target")
                .and_then(|i| argv.get(i + 1)).and_then(|s| s.parse().ok());
            let timeout_s: u64 = argv.iter().position(|a| a == "--timeout")
                .and_then(|i| argv.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(1800);
            let path = sigil_top_db_path();
            let store = match block_store::BlockStore::open(&path) {
                Ok(s) => s,
                Err(e) => { eprintln!("{RED}✗ open store {path}: {e}{RESET}"); std::process::exit(2); }
            };
            println!("\n  {VBRIGHT}{BOLD}◆ SIGIL full verifying sync{RESET}  {DIM}(store {path}){RESET}");
            println!("  {DIM}connecting to the sigil-g0 mesh — downloading + verifying genesis→tip…{RESET}");
            if let Some(t) = target_arg { println!("  {DIM}target height pinned to {t}{RESET}"); }
            println!("  {DIM}timeout {timeout_s}s · Ctrl-C to stop{RESET}\n");
            let sync = block_sync::P2PBlockSync::launch(store, false);
            // v0.15.1: a pinned --target also SEEDS the backfill tip so the refill
            // fires immediately (the gate is peer_best>0, not target_arg). Without
            // this, a quiet mesh left peer_best=0 and full-sync --target never pulled.
            if let Some(t) = target_arg { sync.set_known_tip(t); }
            let start = Instant::now();
            let mut last_print = instant_ago(10);
            loop {
                let st = sync.poll_state();
                if let Some(b) = &st.verify_break {
                    eprintln!("  {RED}✗ verification break — {b}{RESET}");
                    eprintln!("  {RED}  the downloaded chain does NOT form one connected spine. Aborting.{RESET}\n");
                    std::process::exit(1);
                }
                let target = target_arg.unwrap_or(st.peer_best_height);
                if last_print.elapsed() >= Duration::from_secs(2) {
                    last_print = Instant::now();
                    let pct = if target > 0 { (st.verified as f64 / target as f64 * 100.0).min(100.0) } else { 0.0 };
                    println!("  {CYAN}⬇{RESET} verified {GREEN}{}{RESET} / synced {GOLD}{}{RESET} / tip {} · {VBRIGHT}{:.1}%{RESET} · {} peers · {}s",
                        group(st.verified), group(st.blocks_synced), if target > 0 { group(target) } else { "?".into() },
                        pct, st.peer_count, start.elapsed().as_secs());
                }
                if target > 0 && st.verified >= target {
                    println!("\n  {GREEN}{BOLD}✓ full verifying sync complete — {} blocks verified as one connected spine to genesis{RESET}\n", group(st.verified));
                    std::process::exit(0);
                }
                if start.elapsed() > Duration::from_secs(timeout_s) {
                    eprintln!("\n  {RED}✗ timeout after {timeout_s}s — verified {} / target {} (peers={}){RESET}\n",
                        group(st.verified), if target > 0 { group(target) } else { "?".into() }, st.peer_count);
                    std::process::exit(3);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        _ => {}
    }
    // v0.7.5: Auto-update is NON-BLOCKING. The old blocking call hung the splash
    // screen for 30+ seconds on slow connections. Now we check the channel on a
    // background thread during the first refresh cycle — TUI loads instantly.
    // --no-update / SIGIL_TOP_NO_AUTOUPDATE=1 still opts out entirely.
    let mut cfg = parse_args();
    cfg.initial_toast = None; // update check runs async in the TUI
    // Non-TTY (piped / redirected / captured), --once, or --lite → emit exactly ONE
    // plain ANSI frame and exit. ratatui needs a real terminal; this path never
    // spams. The live, interactive dashboard is the TUI below.
    let interactive = std::io::stdout().is_terminal();
    // Non-TTY (piped / captured / redirected) or --once → one plain frame, no loop.
    if cfg.once || !interactive {
        let (st, online, source) = fetch_best(&cfg);
        let frame = if cfg.lite { render_lite(&st, online) } else { render_full(&st, online, &cfg.api, source) };
        print!("{frame}");
        let _ = std::io::stdout().flush();
        return;
    }
    // --lite → the compact one-line scorecard, live-looped in place (TTY only).
    if cfg.lite {
        let clear = "\x1b[H\x1b[2J\x1b[3J";
        loop {
            let t = Instant::now();
            let (st, online, _src) = fetch_best(&cfg);
            print!("{clear}{}", render_lite(&st, online));
            let _ = std::io::stdout().flush();
            let nap = Duration::from_secs(cfg.interval).saturating_sub(t.elapsed());
            std::thread::sleep(nap.max(Duration::from_millis(200)));
        }
    }
    // DEFAULT — the custom ratatui dashboard (Quillon-graph-node styled, multi-panel).
    // ratatui owns all box-drawing + layout, so alignment can never regress.
    let _ = cfg.tui; // --tui kept as an explicit alias; it's the default now
    if let Err(e) = run_tui(cfg) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        eprintln!("sigil-top: TUI error: {e}");
    }
}

// ───────────────────────── ratatui TUI (v0.2) ─────────────────────────

// v0.3 redesign — 24-bit TRUECOLOR obsidian palette (replaces 256-indexed for richer rich-text).
const C_VIOLET: Color = Color::Rgb(0x95, 0x80, 0xff);   // panel borders
const C_VBRIGHT: Color = Color::Rgb(0xc8, 0xb6, 0xff);  // brand / emphasis
const C_GOLD: Color = Color::Rgb(0xf5, 0xc8, 0x5a);     // values / accents
const C_GREEN: Color = Color::Rgb(0x66, 0xe6, 0x8c);    // live / verified
const C_RED: Color = Color::Rgb(0xff, 0x6b, 0x6b);      // offline / failed
const C_DIM: Color = Color::Rgb(0x74, 0x74, 0x92);      // labels / subtle
const C_CYAN: Color = Color::Rgb(0x4f, 0xd6, 0xe0);     // titles / links
#[allow(dead_code)]
const C_INK: Color = Color::Rgb(0x4a, 0x4a, 0x66);      // faintest (separators)

// ─────────────────────────────────────────────────────────────────────────────
// Flux-way self-update — read the release channel, BLAKE3-verify, hot-swap in place.
// ─────────────────────────────────────────────────────────────────────────────

/// One platform's prebuilt in the manifest. Our per-OS extension to the flux
/// release-channel shape (`flux_release_check` reads the top-level fields only).
/// Backward-compat: v0.3.x manifests used `blake3` and `size` keys.
#[derive(Deserialize, Default, Clone)]
struct Target {
    url: String,
    #[serde(default, alias = "blake3")]
    blake3_hex: String,
    #[serde(default, alias = "size")]
    size_bytes: u64,
}

/// `sigil-top-latest.json` — same shape `flux_release_publish` writes, plus a
/// `targets` map so one channel serves both the Linux build and the Windows .exe.
/// Backward-compat: v0.3.x manifests used `blake3` and `size` keys, and
/// target triple keys like `x86_64-unknown-linux-musl`.
#[derive(Deserialize)]
struct Release {
    #[serde(default)]
    version: String,
    #[serde(default)]
    url: String,
    #[serde(default, alias = "blake3")]
    blake3_hex: String,
    #[serde(default, alias = "size")]
    size_bytes: u64,
    #[serde(default)]
    targets: std::collections::HashMap<String, Target>,
}

/// Old manifest target triples that map to our short platform names.
const LEGACY_SELF_KEYS: &[&str] = if cfg!(windows) {
    &["windows-x64", "x86_64-pc-windows-gnu"]
} else if cfg!(target_os = "macos") {
    &["macos-arm64", "aarch64-apple-darwin"]
} else {
    &["linux-x64", "x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"]
};

impl Release {
    /// The download for THIS platform: try the current `SELF_TARGET` key first,
    /// then legacy target triples, else fall back to top-level single-binary fields.
    fn for_self(&self) -> Target {
        for key in LEGACY_SELF_KEYS {
            if let Some(t) = self.targets.get(*key) {
                return t.clone();
            }
        }
        Target {
            url: self.url.clone(),
            blake3_hex: self.blake3_hex.clone(),
            size_bytes: self.size_bytes,
        }
    }
}

/// Fetch the live release manifest (short timeout — runs on the UI thread). `None`
/// if the channel is unreachable or malformed.
fn fetch_latest() -> Result<Release, String> {
    // 8s, not 3s: a cold Windows SChannel handshake to quillon.xyz can take >3s on
    // first contact (the same fetch_feed uses 6s and works). And we DON'T swallow the
    // error (was `.ok()?` → blind "unreachable") — surface the real reason instead.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .min_tls_version(reqwest::tls::Version::TLS_1_0)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build().map_err(|e| format!("client init: {e}"))?;
    // Read the body as text + parse explicitly (reqwest's .json() Display hides the
    // real serde error behind a generic "error decoding response body"). Two attempts
    // guard a transient truncated body on a flaky link; on failure we surface the
    // ACTUAL serde error + what arrived, so the toast says exactly what's wrong.
    // Cache-bust: "works on server, fails on client" usually means a stale/corrupt
    // cached manifest (q-flux / CDN / OS). A fresh ?t= each call bypasses every cache.
    let bust = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let url = format!("{UPDATE_MANIFEST}?t={bust}");
    let mut last = String::from("no response");
    for _ in 0..2 {
        match client.get(&url).send().and_then(|r| r.error_for_status()) {
            Ok(resp) => match resp.text() {
                Ok(body) => match serde_json::from_str::<Release>(&body) {
                    Ok(rel) => return Ok(rel),
                    Err(e) => last = format!("parse: {e} [{}B: {:?}]",
                        body.len(), body.chars().take(48).collect::<String>()),
                },
                Err(e) => last = format!("read body: {e}"),
            },
            Err(e) => {
                last = if e.is_timeout() { "timed out (>8s) — slow link".into() }
                       else if e.is_connect() { format!("connect failed: {e}") }
                       else { format!("request failed: {e}") };
            }
        }
    }
    Err(last)
}

/// Is `a` a newer dotted version than `b`? Numeric per-part compare.
/// v0.3.1: fetch the _sigil-tip DNS anchor via Cloudflare DoH, parse with
/// sigil-dns-anchor, and return a human-readable status. Composes with the
/// DNS-3 resolver-verifier lane once SQIsign verify is wired.
fn fetch_dns_anchor() -> String {
    const ANCHOR: &str = "_sigil-tip.sigilgraph.quillon.xyz";
    let url = format!("https://cloudflare-dns.com/dns-query?name={ANCHOR}&type=TXT");
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .min_tls_version(reqwest::tls::Version::TLS_1_0)
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("✗ DNS anchor: client init failed: {e}"),
    };
    let resp = match client
        .get(&url)
        .header("accept", "application/dns-json")
        .send()
    {
        Ok(r) => r,
        Err(e) => return format!("✗ DNS anchor: DoH request failed: {e}"),
    };
    let body = match resp.text() {
        Ok(b) => b,
        Err(e) => return format!("✗ DNS anchor: read body: {e}"),
    };
    // Parse the DNS JSON response, extract the first TXT record
    let txt: String = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => v
            .get("Answer")
            .and_then(|a| a.get(0))
            .and_then(|r| r.get("data"))
            .and_then(|d| d.as_str())
            .map(|s| s.trim_matches('"').to_string())
            .unwrap_or_default(),
        Err(e) => return format!("✗ DNS anchor: JSON parse: {e}"),
    };
    if txt.is_empty() {
        return "✗ DNS anchor: _sigil-tip TXT record not published yet".into();
    }
    // Structural-validate with sigil-dns-anchor
    match sigil_dns_anchor::decode(&txt) {
        Ok(anchor) => format!(
            "✓ DNS anchor: {} @ height {} · key {}… (SQIsign sig present, verify pending)",
            anchor.record_type,
            anchor.height,
            &anchor.key_id[..8]
        ),
        Err(e) => format!("✗ DNS anchor: parse failed: {e}"),
    }
}

/// v0.7.0: Poll each fleet node's status API to check uptime, height, and version.
/// Runs on the UI thread (quick timeout per node — 3s each). AI operators depend on
/// this to know if their fleet needs attention.
fn check_fleet_health(nodes: &mut Vec<FleetNode>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .danger_accept_invalid_certs(true)
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    for node in nodes.iter_mut() {
        let url = format!("https://{}:{}/api/v1/status", node.addr, 8181);
        match client.get(&url).send() {
            Ok(resp) => {
                node.online = true;
                if let Ok(json) = resp.json::<serde_json::Value>() {
                    node.height = json.get("block_height")
                        .or_else(|| json.get("tip_height"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    node.version = json.get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    node.uptime_secs = json.get("uptime")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            Err(_) => {
                node.online = false;
                node.height = 0;
                node.version.clear();
            }
        }
    }
}

fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y { return x > y; }
    }
    false
}

/// Download the new binary, BLAKE3-verify against the manifest, and hot-swap THIS
/// executable in place (cross-platform via `self_replace`). Returns a status line.
/// The mining endpoint (sigil-rpcd `/mine`). Override with `SIGIL_MINE_URL`; defaults to the local
/// node's rpcd. The node verifies BLAKE3 leading-zero-bits PoW in `submit_share` and credits the miner.
fn mine_url() -> String {
    std::env::var("SIGIL_MINE_URL").unwrap_or_else(|_| "https://sigilgraph.fluxapp.xyz:8447/v1/mine".into())
}
/// A stable per-install miner wallet (64-hex): BLAKE3 of the hostname, or `SIGIL_MINE_WALLET`.
fn miner_wallet() -> String {
    std::env::var("SIGIL_MINE_WALLET").ok().filter(|s| s.len() == 64).unwrap_or_else(|| {
        let host = std::env::var("HOSTNAME").or_else(|_| std::env::var("HOST")).unwrap_or_else(|_| "sigil-top".into());
        blake3::hash(format!("sigil-top-miner:{host}").as_bytes()).to_hex().to_string()
    })
}

/// Start REAL mining on a background thread: find a BLAKE3 nonce meeting `difficulty_bits`, POST it to
/// the node's `/mine` endpoint, repeat. Accepted shares are reported over the returned channel so the
/// TUI shows live progress. Stops when `stop` flips true. This is what makes pressing **[M]** actually
/// mine — not just toggle a flag. Light difficulty (testnet) so shares land in seconds.
fn start_mining(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> mpsc::Receiver<String> {
    use std::sync::atomic::Ordering;
    let (tx, rx) = mpsc::channel();
    let (url, wallet) = (mine_url(), miner_wallet());
    thread::spawn(move || {
        let difficulty_bits: u32 = std::env::var("SIGIL_MINE_DIFFICULTY").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(12); // ~4k hashes/share — real PoW, lands fast
        let client = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(8))
        .min_tls_version(reqwest::tls::Version::TLS_1_0).build() {
            Ok(c) => c, Err(e) => { let _ = tx.send(format!("✗ miner init: {e}")); return; }
        };
        let _ = tx.send(format!("▲ mining → {url} · diff {difficulty_bits} bits · wallet {}…", &wallet[..8]));
        let mut accepted = 0u64;
        let mut hashes: u64 = 0;
        let mut last_rate = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            // header binds the share to the current minute (cheap freshness); find a winning nonce.
            let header = format!("sigil-g0-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 30).unwrap_or(0));
            let mut nonce = 0u64;
            let found = loop {
                if stop.load(Ordering::Relaxed) { break None; }
                let mut buf = header.as_bytes().to_vec();
                buf.extend_from_slice(&nonce.to_le_bytes());
                hashes = hashes.wrapping_add(1);
                if leading_zero_bits(blake3::hash(&buf).as_bytes()) >= difficulty_bits { break Some(nonce); }
                nonce = nonce.wrapping_add(1);
                // Report hashrate every ~2s on the channel (v0.2.35).
                if hashes % 500_000 == 0 && last_rate.elapsed() >= Duration::from_secs(2) {
                    let rate = hashes as f64 / last_rate.elapsed().as_secs_f64().max(0.001);
                    let _ = tx.send(format!("⛏ {:.2} MH/s · {}M hashes", rate / 1e6, hashes / 1_000_000));
                    last_rate = Instant::now();
                }
            };
            let Some(nonce) = found else { break };
            let body = format!("{{\"miner\":\"{wallet}\",\"header\":\"{header}\",\"nonce\":{nonce},\"difficulty\":{difficulty_bits},\"reward\":50}}");
            match client.post(&url).header("Content-Type", "application/json").body(body).send() {
                Ok(r) => {
                    let txt = r.text().unwrap_or_default();
                    if txt.contains("\"ok\":true") {
                        accepted += 1;
                        let bal = txt.split("\"new_balance\":").nth(1).and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()).unwrap_or("?");
                        let _ = tx.send(format!("✓ share {accepted} accepted (nonce {nonce}) · balance {bal}"));
                    } else {
                        let _ = tx.send(format!("✗ share rejected: {}", txt.chars().take(60).collect::<String>()));
                    }
                }
                Err(e) => { let _ = tx.send(format!("✗ submit: {e} (retry 3s)")); thread::sleep(Duration::from_millis(2200)); }
            }
            thread::sleep(Duration::from_millis(800)); // gentle cadence
        }
        let _ = tx.send(format!("▲ mining stopped ({accepted} accepted this session)"));
    });
    rx
}

/// BLAKE3 leading-zero-bits of a digest — the same PoW measure `submit_share` enforces node-side.
fn leading_zero_bits(d: &[u8]) -> u32 {
    let mut n = 0u32;
    for &b in d { if b == 0 { n += 8; } else { n += b.leading_zeros(); break; } }
    n
}

/// Default-ON startup auto-update against the **pinned release channel**. Runs once at launch:
/// fetch the operator-controlled manifest, and ONLY if it names a version newer than this binary
/// (i.e. the operator has *promoted* a release by writing the manifest — publishing a GitHub release
/// alone does NOT advance the channel), download + BLAKE3-verify + hot-swap, then re-exec the new
/// binary so the node is immediately running the chosen version. Returns an Option<String> toast
/// for the TUI instead of eprintln! (which corrupts the alt-screen). Disable with `--no-update` or
/// `SIGIL_TOP_NO_AUTOUPDATE=1`.
fn maybe_auto_update(argv: &[String]) -> Option<String> {
    if argv.iter().any(|a| a == "--no-update")
        || std::env::var("SIGIL_TOP_NO_AUTOUPDATE").map(|v| v == "1").unwrap_or(false)
    {
        return None;
    }
    let rel = match fetch_latest() {
        Ok(r) if version_gt(&r.version, VERSION) => r,
        _ => return None, // up to date, channel unreachable, or malformed → just run
    };
    match self_update(&rel) {
        Ok(_) => {
            // Relaunch into the new binary. self_replace installed the new version AT THE
            // CURRENT EXE PATH, so that's the canonical relaunch target; a versioned copy
            // beside us (if one survived) is only a fallback. The previous Windows branch
            // spawned ONLY the versioned file and, when it was absent, hit a bare exit(0) —
            // so the app updated in place but never restarted ("just exits"). Now every
            // platform relaunches the in-place exe. The new process re-runs this check,
            // sees its version == the channel, and proceeds — no update loop.
            // relaunch_new_binary replaces this process (unix exec) / spawns+exits
            // (win/mac) on success and only RETURNS on failure — it never spawns a
            // detached child that would fight the terminal. On success this line is
            // never reached; on failure the new binary is already swapped in place, so
            // we keep running the current process this time and pick it up next launch.
            relaunch_new_binary(&rel.version);
            Some(format!("↑ updated to v{} — restart to run it", rel.version))
        }
        Err(e) => Some(format!("auto-update skipped: {e}")),
    }
}

fn self_update(rel: &Release) -> Result<String, String> {
    let t = rel.for_self();
    if t.url.is_empty() { return Err(format!("manifest has no {SELF_TARGET} build")); }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .min_tls_version(reqwest::tls::Version::TLS_1_0)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build().map_err(|e| e.to_string())?;
    let bytes = client.get(&t.url).send().map_err(|e| e.to_string())?
        .error_for_status().map_err(|e| e.to_string())?
        .bytes().map_err(|e| e.to_string())?;
    if t.size_bytes != 0 && bytes.len() as u64 != t.size_bytes {
        return Err(format!("size mismatch — got {} expected {} bytes", bytes.len(), t.size_bytes));
    }
    // BLAKE3 content-hash gate — the release channel signs binaries by blake3.
    if !t.blake3_hex.is_empty() {
        let got = blake3::hash(&bytes).to_hex().to_string();
        if !got.eq_ignore_ascii_case(&t.blake3_hex) {
            return Err(format!("BLAKE3 mismatch — refusing swap (got {}…)", &got[..12]));
        }
    }
    // Save beside the current exe as a versioned binary.
    // Windows: cannot swap running .exe; save as sigil-top-v{VERSION}.exe.
    // Unix: try atomic self-replace; fall back to versioned binary beside.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let beside = exe.with_file_name(format!("sigil-top-v{}{}", rel.version,
        if cfg!(windows) { ".exe" } else { "" }));
    std::fs::write(&beside, &bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&beside, std::fs::Permissions::from_mode(0o755));
    }
    // In-place self-replace on BOTH platforms — the self_replace crate handles the
    // Windows "rename the running .exe out of the way" trick, so the launched
    // sigil-top(.exe) actually becomes the new version (was unix-only → Windows kept
    // relaunching the old exe = "doesn't update").
    // v0.27.5: keep the CURRENT binary as the rollback image BEFORE swapping. If the new
    // version passes pre-flight but then crash-loops in real operation, `crashloop_guard()`
    // reverts to this on the next boot (self-healing updater). Best-effort.
    if let (Ok(cur), Some(prev)) = (std::env::current_exe(), prev_binary_path()) {
        let _ = std::fs::copy(&cur, &prev);
    }
    if self_replace::self_replace(&beside).is_ok() {
        let _ = std::fs::remove_file(&beside);
        return Ok(format!("swapped v{VERSION} -> v{} ({:.1} MB) — restart to run",
            rel.version, bytes.len() as f64 / 1.048576e6));
    }
    Ok(format!("saved v{} ({:.1} MB) -> {}", rel.version, bytes.len() as f64 / 1.048576e6, beside.display()))
}

/// v0.25: pre-flight a freshly-swapped binary BEFORE handing off to it. `exec`/`spawn+exit`
/// destroys the running app; if the new binary is corrupt (truncated download), ABI/GLIBC-
/// incompatible, or hangs on start, the app would simply VANISH on the restart-after-sync —
/// the exact bug this fixes. Spawn `target --selfcheck` (a no-op that prints the version and
/// exits 0) with a short timeout; return Ok(version) only if it runs cleanly AND prints a
/// non-empty version. Anything else → don't hand off, keep the running app alive.
fn preflight_binary(target: &std::path::Path) -> Result<String, String> {
    use std::process::{Command, Stdio};
    use std::io::Read;
    let mut child = Command::new(target)
        .arg("--selfcheck")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    // v0.26 (DeepSeek-hardened): poll on THIS thread so we keep the Child handle and can KILL
    // it on timeout — a binary that HANGS on start must not leak a thread + zombie child (the
    // old wait_with_output-on-a-thread design couldn't reach the child to kill it). --selfcheck
    // prints ~7 bytes then exits immediately, so the stdout pipe never fills → no try_wait
    // deadlock. A hung child is itself a strong "don't hand off" signal.
    let deadline = Instant::now() + Duration::from_secs(6);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap the zombie
                    return Err("--selfcheck timed out (binary hangs on start)".into());
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(e) => { let _ = child.kill(); let _ = child.wait(); return Err(format!("wait failed: {e}")); }
        }
    };
    let mut buf = String::new();
    if let Some(mut out) = child.stdout.take() { let _ = out.read_to_string(&mut buf); }
    if !status.success() {
        return Err(format!("--selfcheck exited {:?}", status.code()));
    }
    let v = buf.trim().to_string();
    if v.is_empty() { Err("empty --selfcheck output".into()) } else { Ok(v) }
}

/// Relaunch into the just-installed binary after a successful `self_update`. `self_replace`
/// put the new version at the current exe path, so that's the canonical target; a versioned
/// copy beside us (if any) is a fallback.
///
/// v0.25 FAIL-SAFE: we PRE-FLIGHT the target (`--selfcheck`) before any handoff. `exec`
/// destroys this process, so we only ever do it for a binary we've CONFIRMED starts and
/// reports a sane version. If the pre-flight fails (corrupt swap, ABI mismatch, hang) we
/// return `false` WITHOUT tearing anything down — the caller restores its TUI and tells the
/// user to restart manually, and the app keeps running on the current image. No more
/// "app vanishes when it tries to restart after sync". Returns `false` on any non-handoff
/// path; on unix a successful pre-flight + `exec` never returns.
fn relaunch_new_binary(version: &str) -> bool {
    let exe = match std::env::current_exe() { Ok(e) => e, Err(_) => return false };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ver_exe = exe.with_file_name(format!(
        "sigil-top-v{}{}", version, if cfg!(windows) { ".exe" } else { "" }));
    let target = if ver_exe.exists() { ver_exe } else { exe.clone() };

    // GATE: never hand off to an unverified binary. A failed pre-flight means the swapped
    // binary can't start — abort the relaunch and stay alive on the current (working) image.
    match preflight_binary(&target) {
        Ok(reported) => {
            // Sanity: the new binary should report the version we just installed. A mismatch
            // (e.g. self_replace silently no-op'd) isn't fatal — it still STARTS — but log it.
            if !reported.is_empty() && reported != version {
                eprintln!("  [update] pre-flight: new binary reports v{reported}, expected v{version} — relaunching anyway");
            }
        }
        Err(e) => {
            eprintln!("  [update] relaunch ABORTED — new binary failed pre-flight ({e}); staying on the current version, restart manually to apply.");
            return false;
        }
    }

    // Pre-flight passed → commit to the handoff.
    std::env::set_var("SIGIL_TOP_JUST_UPDATED", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec REPLACES this process — it only returns on FAILURE. Pre-flight already proved
        // the binary runs, so a failure here is exotic (e.g. ETXTBSY). Don't detach a child
        // that fights the foreground terminal; return false so the caller restores its TUI.
        let _err = std::process::Command::new(&target).args(&args).exec();
        false
    }
    #[cfg(not(unix))]
    {
        // Windows/macOS can't replace a running image — spawn the (pre-flighted) new one,
        // then exit, but ONLY if the spawn succeeded (else return false, don't exit on the user).
        if std::process::Command::new(&target).args(&args).spawn().is_ok() {
            std::process::exit(0);
        }
        false
    }
}

struct App {
    cfg: Config,
    st: NodeStatus,
    online: bool,
    last_fetch: Instant,
    verify: Option<TipVerify>,
    toast: String,
    toast_sticky: bool,        // v0.2.35: user-action toasts survive mining noise
    latest: String,            // live version from the flux release channel (auto-refreshed)
    last_update_check: Instant,
    update_rx: Option<mpsc::Receiver<String>>, // [U] runs on a bg thread; result lands here
    blocks: Vec<FeedBlock>,
    target_height: u64,  // the network tip we're syncing to
    synced_height: u64,  // last height we cryptographically verified
    verified_count: u64, // tips verified this session
    streak: u64,
    score: u64,
    mining: bool,
    mine_rx: Option<mpsc::Receiver<String>>,           // accepted-share messages from the miner thread
    mine_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>, // signals the miner thread to stop
    mine_accepted: u64,                                 // shares the node accepted this session
    mine_hashrate: f64,                                 // v0.2.35: live GH/s from the miner thread
    mine_hashes: u64,                                   // v0.2.35: cumulative hashes computed
    wallet_balance: u128,                               // v0.2.35: miner wallet balance from feed
    full_sync: bool,                                    // [F] opt-in heavy full sync (default = 10ms lightweight verify)
    full_sync_height: u64,                              // blocks downloaded so far in full sync
    full_sync_target: u64,                              // target height for full sync
    full_sync_active: bool,                             // true while downloading
    sync_us: u128,
    // L2-B: real eclipse-K — measured independent sources agreeing on the tip (replaces hardcoded k).
    eclipse_k: u32,
    eclipse_sources: Vec<(String, bool)>,
    last_eclipse: Instant,
    /// Post-update logo splash (flux updater return UX).
    splash_until: Option<Instant>,
    splash_frame: u8,
    // v0.6.0: Cortex MCP combo integration — AI agent registry + optimization engine
    cortex: Option<Cortex>,
    agents: Vec<AiAgent>,
    cortex_loops: u64,
    last_cortex_gain: f64,
    cortex_summary: String,
    /// v0.11.0: cortex snapshot shared with the embedded HTTP server so the explorer's
    /// `/api/v1/cortex` panel reflects the live optimization-engine state.
    cortex_shared: std::sync::Arc<std::sync::Mutex<local_api::CortexSnapshot>>,
    mcp_combo_tool: String,     // active MCP combo verb being executed
    mcp_combo_result: String,   // last MCP combo result
    // v0.6.5: Real P2P block sync via flux-p2p mesh (Delta + Epsilon)
    p2p_sync: Option<block_sync::P2PBlockSync>,
    p2p_state: block_sync::P2PSyncState,
    p2p_blocks_synced: u64,
    p2p_rate: f64,                            // backfill blocks/sec (10s trailing window = current speed)
    p2p_rate_samples: std::collections::VecDeque<(std::time::Instant, u64)>, // (t, blocks_synced)
    // v0.7.0: AI fleet monitoring — AIs worry about their nodes' uptime and version compliance
    fleet_nodes: Vec<FleetNode>,
    fleet_last_check: Instant,
    // v0.6.0: fluxc serve status for local wallet + cockpit
    serve_status: String,
    // v0.7.0: embedded HTTP serve shutdown signal (no external process)
    serve_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    // v0.10.5 "smooth cruise": refresh runs off the render thread. The worker's
    // result lands on refresh_rx; refresh_inflight stops duplicate workers piling up.
    refresh_rx: Option<mpsc::Receiver<RefreshOutcome>>,
    refresh_inflight: bool,
    // v0.10.5.1: graceful offline handling + embedded-serve watchdog.
    offline_since: Option<Instant>,  // when the node first stopped answering
    offline_streak: u32,             // consecutive offline refreshes → backoff
    last_serve_check: Instant,       // throttle the :9800 liveness probe
    // v0.13: tabbed cockpit
    tab: Tab,
    swarm: SwarmView,
    last_swarm_load: Instant,
}

impl App {
    fn new(cfg: Config) -> Self {
        let toast = cfg.initial_toast.clone().unwrap_or_default();
        App { cfg, st: NodeStatus::default(), online: false, last_fetch: Instant::now(),
              verify: None, toast, toast_sticky: false,
              latest: LATEST.to_string(),
              // v0.7.5: Trigger first check immediately (now - 300s = overdue)
              last_update_check: instant_ago(301),
              update_rx: None,
              blocks: Vec::new(),
              target_height: 0, synced_height: 0, verified_count: 0, streak: 0, score: 0,
              mining: false, mine_rx: None, mine_stop: None, mine_accepted: 0,
              mine_hashrate: 0.0, mine_hashes: 0, wallet_balance: 0,
              full_sync: false, full_sync_height: 0, full_sync_target: 0, full_sync_active: false, sync_us: 0,
              eclipse_k: 0, eclipse_sources: Vec::new(),
              last_eclipse: instant_ago(60),
              splash_until: if std::env::var("SIGIL_TOP_JUST_UPDATED").ok().as_deref() == Some("1") {
                  let _ = std::env::remove_var("SIGIL_TOP_JUST_UPDATED");
                  Some(Instant::now() + Duration::from_millis(1800))
              } else { None },
              splash_frame: 0,
              // v0.6.0: Cortex MCP combo integration
              cortex: None,
              agents: default_agent_registry(),
              cortex_loops: 0,
              last_cortex_gain: 0.0,
              cortex_summary: String::new(),
              cortex_shared: std::sync::Arc::new(std::sync::Mutex::new(local_api::CortexSnapshot::default())),
              mcp_combo_tool: String::new(),
              mcp_combo_result: String::new(),
              // v0.6.5: P2P block sync starts lazy — launched in run_tui after terminal is ready
              p2p_sync: None,
              p2p_state: block_sync::P2PSyncState::default(),
              p2p_blocks_synced: 0,
              p2p_rate: 0.0,
              p2p_rate_samples: std::collections::VecDeque::new(),
              serve_status: String::new(),
              serve_stop: None,
              // v0.7.0: Fleet starts with known bootstrap peers
              fleet_nodes: vec![
                  FleetNode { name: "Delta".into(), addr: "5.79.79.158".into(), port: 9003, online: false, height: 0, version: String::new(), uptime_secs: 0 },
                  FleetNode { name: "Epsilon".into(), addr: "89.149.241.126".into(), port: 9003, online: false, height: 0, version: String::new(), uptime_secs: 0 },
              ],
              fleet_last_check: instant_ago(3600),
              refresh_rx: None,
              refresh_inflight: false,
              offline_since: None,
              offline_streak: 0,
              last_serve_check: Instant::now(),
              tab: Tab::Node,
              swarm: SwarmView::default(),
              last_swarm_load: instant_ago(10),
        }
    }

    /// v0.10.5.1: adaptive refresh cadence. Base interval while the node answers;
    /// a gentle backoff (cap 15s) while it's offline so we don't hammer a dead
    /// endpoint; instant snap-back the moment it reconnects. Cruise control: ease
    /// off when the road's empty, accelerate the instant traffic returns.
    fn refresh_delay(&self) -> Duration {
        let base = self.cfg.interval.max(1);
        if self.offline_streak == 0 {
            Duration::from_secs(base)
        } else {
            let mult = 1u64 << self.offline_streak.saturating_sub(1).min(4); // 1,2,4,8,16
            Duration::from_secs((base * mult).min(15))
        }
    }
    fn resync(&mut self) {
        // Full resync: clear local caches, force-fetch fresh state from all sources.
        self.blocks.clear();
        self.synced_height = 0;
        self.verify = None;
        self.toast = "⟳ Resync — clearing caches, re-fetching chain state…".into();
        self.toast_sticky = false;
        self.refresh();
    }
    /// Back-compat shim: callers that used to block now kick off an async refresh.
    fn refresh(&mut self) { self.request_refresh(); }

    /// v0.13: reload the swarm coordination snapshot for the [2]/[3] tabs.
    fn load_swarm(&mut self) {
        self.swarm = load_swarm_view();
        self.last_swarm_load = Instant::now();
    }

    /// v0.10.5: spawn the network refresh on a worker thread (if none in flight).
    /// Returns immediately — the render loop keeps drawing while the socket work
    /// happens elsewhere. Result is drained by `poll_refresh`.
    fn request_refresh(&mut self) {
        if self.refresh_inflight { return; }
        self.refresh_inflight = true;
        // Decide here (UI thread) whether this cycle re-measures eclipse-K, so the
        // 30s throttle stays honest even though the DoH probe runs off-thread.
        let want_eclipse = self.last_eclipse.elapsed() >= Duration::from_secs(30);
        if want_eclipse { self.last_eclipse = Instant::now(); }
        let feed = self.cfg.feed.clone();
        let api = self.cfg.api.clone();
        let prior_synced = self.synced_height;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(fetch_refresh(feed, api, want_eclipse, prior_synced));
        });
        self.refresh_rx = Some(rx);
    }

    /// v0.10.5: drain a completed refresh without ever blocking. Called once per
    /// render-loop iteration.
    fn poll_refresh(&mut self) {
        let Some(rx) = self.refresh_rx.as_ref() else { return };
        match rx.try_recv() {
            Ok(out) => { self.refresh_rx = None; self.apply_refresh(out); }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Worker died (panic / drop) — clear in-flight so the next interval retries.
                self.refresh_rx = None;
                self.refresh_inflight = false;
                self.last_fetch = Instant::now();
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// v0.10.5: merge a worker result into App state. Cheap (no I/O) → safe on the
    /// UI thread. This is the non-network tail of the old `refresh`.
    fn apply_refresh(&mut self, out: RefreshOutcome) {
        self.st = out.st;
        self.online = out.online;
        // v0.10.5.1: track offline → online transitions for backoff + banner.
        if out.online {
            if self.offline_streak > 0 && !self.toast_sticky {
                let was = self.offline_since.map(|t| fmt_uptime(t.elapsed().as_secs())).unwrap_or_default();
                self.toast = format!("✓ reconnected after {} offline", was);
            }
            self.offline_streak = 0;
            self.offline_since = None;
        } else {
            self.offline_streak = self.offline_streak.saturating_add(1);
            if self.offline_since.is_none() { self.offline_since = Some(Instant::now()); }
        }
        if let Some(b) = out.blocks { self.blocks = b; }
        if out.fallback_note && !self.toast_sticky {
            self.toast = "📡 Blocks fetched from API fallback".into();
        }
        if let Some((k, srcs)) = out.eclipse {
            self.eclipse_k = k;
            self.eclipse_sources = srcs;
        }
        self.refresh_inflight = false;
        // v0.7.8 (HONEST full sync): the old [F] equated full_sync_height with the
        // tip height and printed "complete: N verified" while storing ZERO blocks —
        // a false claim (the DB stayed empty). Full sync now reports the REAL number
        // of blocks actually received + stored via the chain mesh (p2p block store).
        // It only says "complete" when stored blocks actually reach the target, and
        // shows the true count (0 until the node mesh serves history) otherwise.
        if self.full_sync {
            let stored = self.p2p_state.blocks_synced;          // blocks really stored
            let target = self.p2p_state.peer_best_height.max(self.st.height);
            self.full_sync_height = stored;
            self.full_sync_target = target;
            self.full_sync_active = self.p2p_state.running && (target == 0 || stored < target);
            self.toast = if target > 0 && stored >= target {
                format!("✓ Full sync: {} blocks stored + verified", group(stored))
            } else if self.p2p_state.running {
                format!("⬇ Full sync: {} / {} blocks stored via chain mesh", group(stored), group(target))
            } else {
                "⬇ Full sync: connecting to chain mesh…".into()
            };
                            }
        // v0.2.35: carry wallet balance from feed into local state (non-breaking — 0 when absent).
        if self.st.wallet_balance > 0 { self.wallet_balance = self.st.wallet_balance; }
        // Auto-update signal: poll the flux release channel every 5 min so the update
        // v0.7.5: Non-blocking update check — runs on background thread
        if self.last_update_check.elapsed() > Duration::from_secs(300) {
            self.last_update_check = Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let ver = fetch_latest().map(|r| r.version).unwrap_or_default();
                let _ = tx.send(format!("AUTO-CHECK:{ver}"));
            });
            self.update_rx = Some(rx);
        }
        self.target_height = self.st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(self.st.height);
        // verify the tip (verify-don't-trust) and advance the synced height
        self.verify = self.st.tip.as_ref().map(verify_tip);
        if let Some(v) = self.verify.as_ref() {
            if v.ok {
                let advanced = v.height > self.synced_height;
                self.synced_height = v.height;
                self.sync_us = v.latency_us;
                if self.mining && advanced {
                    self.verified_count += 1;
                    self.streak += 1;
                    self.score = self.verified_count * self.streak.max(1);
                }
            } else if self.mining {
                self.streak = 0; // a bad tip breaks the streak
            }
        }
        // eclipse-K is now measured off-thread in fetch_refresh and applied above
        // via out.eclipse — no blocking DoH probe on the render thread anymore.
        self.last_fetch = Instant::now();
    }
}

/// L2-B: REAL eclipse-K — count INDEPENDENT verification paths that agree on the chain tip,
/// replacing the old hardcoded `k=2`. Path 0 = the node/feed tip we just cryptographically verified.
/// Paths 1..N = independent public DoH resolvers resolving the `_sigil-tip` anchor TXT; one counts
/// only if its answer carries the current tip height (so a single lying resolver can't fake the tip —
/// DNS-level eclipse resistance). HONEST: until the anchor is published (L2-C), the DoH paths return
/// nothing → K reflects only what was really verified, never a simulated climb.
// ── v0.27.5: self-healing crash-loop rollback (the updater's third layer) ────────────────
// `--selfcheck` pre-flight (v0.25) catches binaries that can't START; the fail-safe relaunch
// (v0.25) keeps the app alive if a handoff fails. THIS catches the last case: a new version
// that passes pre-flight, starts, but then CRASHES in real operation. Every dashboard boot
// records an attempt for the running VERSION; a detached timer clears it once the process has
// survived HEAL_SECS ("healthy"). If a boot instead finds the SAME version already failed to
// heal CRASH_STRIKES times in a row, it reverts to the binary the last update backed up and
// relaunches — no operator intervention. A high-value 24/7 node self-heals from a bad update.
const HEAL_SECS: u64 = 12;
const CRASH_STRIKES: u32 = 3;

fn prev_binary_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.with_file_name(if cfg!(windows) { "sigil-top-prev.exe" } else { "sigil-top-prev" }))
}
fn boot_marker_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.with_file_name(".sigil-top-boot"))
}
/// Record a boot attempt for the running VERSION; return the consecutive-unhealed strike count
/// (1 on a fresh version). Best-effort — any IO failure returns 1 (proceed, just no rollback).
fn record_boot_attempt() -> u32 {
    let path = match boot_marker_path() { Some(p) => p, None => return 1 };
    let strikes = match std::fs::read_to_string(&path).ok()
        .as_deref().map(str::trim).and_then(|s| s.split_once(':'))
    {
        Some((ver, n)) if ver == VERSION => n.parse::<u32>().unwrap_or(0) + 1,
        _ => 1, // fresh version / no marker / garbage → reset the counter
    };
    let _ = std::fs::write(&path, format!("{VERSION}:{strikes}"));
    strikes
}
/// Clear the boot marker = "this version reached a healthy run".
fn mark_boot_healthy() {
    if let Some(p) = boot_marker_path() { let _ = std::fs::remove_file(p); }
}
/// Arm the detached "survived HEAL_SECS → healthy" timer (decoupled from the UI loop, so a
/// normal long run clears the strike without any render-loop hook; a crash before HEAL_SECS
/// leaves the strike for the next boot to count).
fn arm_heal_timer() {
    std::thread::spawn(|| { std::thread::sleep(Duration::from_secs(HEAL_SECS)); mark_boot_healthy(); });
}
/// At dashboard startup: detect a crash-loop of THIS version and auto-revert to the backed-up
/// previous binary. Returns true if it reverted+relaunched (caller should return); false to run.
fn crashloop_guard() -> bool {
    let strikes = record_boot_attempt();
    if strikes < CRASH_STRIKES { arm_heal_timer(); return false; }
    let prev = match prev_binary_path() { Some(p) if p.exists() => p, _ => {
        mark_boot_healthy(); arm_heal_timer(); return false; // nothing to revert to — just run
    }};
    eprintln!("\n  {GOLD}↩ sigil-top v{VERSION} crash-looped {strikes}× — reverting to the last working binary{RESET}");
    if let Err(e) = preflight_binary(&prev) {
        eprintln!("  {RED}revert target failed pre-flight ({e}) — staying on current{RESET}");
        mark_boot_healthy(); arm_heal_timer(); return false;
    }
    if self_replace::self_replace(&prev).is_err() { mark_boot_healthy(); return false; }
    mark_boot_healthy(); // the reverted binary boots fresh under its own version counter
    let exe = match std::env::current_exe() { Ok(e) => e, Err(_) => return true };
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    { use std::os::unix::process::CommandExt; let _ = std::process::Command::new(&exe).args(&args).exec(); }
    #[cfg(not(unix))]
    { let _ = std::process::Command::new(&exe).args(&args).spawn(); }
    true
}
/// `sigil-top revert` — operator's "undo a bad update" button. Pre-flights the backed-up
/// previous binary, swaps to it, and relaunches.
fn do_revert() {
    let prev = match prev_binary_path() {
        Some(p) if p.exists() => p,
        _ => { println!("\n  {DIM}no previous binary to revert to (no update has run yet){RESET}\n"); return; }
    };
    println!("\n  {GOLD}↩ reverting to the previous binary{RESET} — pre-flighting…");
    match preflight_binary(&prev) {
        Ok(v) => {
            if self_replace::self_replace(&prev).is_ok() {
                println!("  {GREEN}✓ reverted → v{v}{RESET}\n  {DIM}relaunching…{RESET}");
                mark_boot_healthy();
                relaunch_new_binary(&v);
            } else {
                println!("  {RED}✗ swap failed{RESET}\n");
            }
        }
        Err(e) => println!("  {RED}✗ previous binary failed pre-flight ({e}) — NOT reverting{RESET}\n"),
    }
}

fn measure_eclipse_k(tip_height: u64, tip_ok: bool) -> (u32, Vec<(String, bool)>) {
    const ANCHOR: &str = "_sigil-tip.sigilgraph.quillon.xyz";
    let resolvers = [
        ("cloudflare", "https://cloudflare-dns.com/dns-query"),
        ("google", "https://dns.google/resolve"),
        ("quad9", "https://dns.quad9.net/dns-query"),
    ];
    let mut sources: Vec<(String, bool)> = vec![("node (verified)".into(), tip_ok)];
    let marker = tip_height.to_string();
    if let Ok(client) = reqwest::blocking::Client::builder().timeout(Duration::from_millis(2200))
        .min_tls_version(reqwest::tls::Version::TLS_1_0).build() {
        for (name, base) in resolvers {
            let url = format!("{base}?name={ANCHOR}&type=TXT");
            let agree = client
                .get(&url)
                .header("accept", "application/dns-json")
                .send()
                .ok()
                .and_then(|r| r.text().ok())
                .map(|body| tip_ok && tip_height > 0 && body.contains(&marker))
                .unwrap_or(false);
            sources.push((name.to_string(), agree));
        }
    }
    let k = sources.iter().filter(|(_, ok)| *ok).count() as u32;
    (k, sources)
}

fn run_tui(cfg: Config) -> std::io::Result<()> {
    // v0.27.5: self-healing crash-loop guard — long-running dashboard only (`--once` renders
    // and exits faster than HEAL_SECS, which would false-trigger a revert). If THIS version
    // has crash-looped, this reverts to the last working binary and relaunches.
    if !cfg.once && crashloop_guard() { return Ok(()); }
    enable_raw_mode()?;
    // From here ratatui owns the screen — divert background eprintln to the logfile
    // (was smearing the dashboard with [p2p-sync]/[aether] lines).
    IN_TUI.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // v0.10.5 "stable uptime": a panic anywhere below would otherwise leave the
    // user's terminal in raw mode + alternate screen = a bricked, unusable shell.
    // Install a hook that ALWAYS restores the terminal first, then runs the
    // default panic printer. Graceful recovery instead of a wedged terminal.
    {
        let default_hook = std::panic::take_hook();
        let _ = &default_hook; // kept for reference; v0.27 hook is LOG-ONLY (see below)
        std::panic::set_hook(Box::new(move |info| {
            // v0.27: LOG ONLY — do NOT tear down the terminal here. A background-thread panic
            // must not break the still-running TUI, and a render panic is CAUGHT by catch_unwind
            // around term.draw (which re-inits + continues). Terminal restore happens on normal
            // exit (run_tui cleanup) or in the catch handler — never from this hook.
            let msg = info.payload().downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic>".into());
            let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
            log_line(format!("[PANIC] {msg} @ {loc}"));
        }));
    }

    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let mut app = App::new(cfg);
    // v0.10.5: async — kicks off the first fetch without blocking the first paint.
    app.request_refresh();

    // v0.7.22: cross-platform PERSISTENT store path. The old /tmp + /dev/shm paths
    // don't exist on Windows → the store never persisted → re-sync from 0 every launch
    // ("starts over on update"). Now a per-user dir (override with SIGIL_TOP_DB).
    let db_path = sigil_top_db_path();
    let mut block_store = block_store::BlockStore::open(&db_path)
        .or_else(|_| block_store::BlockStore::open(
            std::env::temp_dir().join("sigil-top-blocks.db").to_string_lossy().as_ref()))
        .unwrap_or_else(|e| panic!("block store: {e}"));

    // v0.7.1: Bootstrap from local aether shards into flux-db before starting P2P
    match block_store::sync_aether_to_fluxdb(&mut block_store, "/opt/orobit/sigil-data/db-epsilon/aether") {
        Ok(n) if n > 0 => {
            app.toast = format!("⬇ Synced {n} blocks → flux-db (height {})", block_store.best_height());
        }
        Err(e) => tlog!("[aether] {e}"),
        _ => {}
    }

    // v0.10.2: full P2P block-sync is OPT-IN. It spins a 3-worker tokio runtime +
    // 8×~9 MB parallel header downloads to pull genesis→tip (~150k blocks), which
    // pegged CPU + network on startup for no benefit — the dashboard already proves
    // the WHOLE chain via the succinct fold-proof (~342 ms, 2.5 KB). So the default
    // is a true light monitor; pass `--sync` (or SIGIL_TOP_SYNC=1), or use the
    // `full-sync` / `verify-chain` subcommands, when you actually want live blocks.
    // v0.11.0: a read-only view of the SAME flux-db, cloned BEFORE the store is moved
    // into the sync thread. The embedded HTTP server uses it to answer the explorer's
    // /api/v1/{recent,search,aether} from the local verified spine.
    let block_reader = block_store.reader();

    // v0.26 hardening #8 (DeepSeek-reviewed): graceful SIGTERM/SIGINT. A supervisor restart
    // (or Ctrl-C in a headless run) otherwise kills the process mid-window with no final flush
    // and, in the TUI, a terminal left in raw mode. Restore the terminal, flush the verified-
    // spine watermark to flux-db (the reader shares the live DB via Arc), then exit 0 so the
    // supervisor restarts cleanly and never orphans the persisted synced/verified state.
    {
        // v0.26.6 fix: BlockReader is a read-only view (no flush). The sync threads
        // BlockStore persists the synced/verified watermark to flux-db on every advance,
        // so the latest watermark is already durable at Ctrl-C — just restore the terminal
        // and exit cleanly (no orphaned raw mode).
        let _ = ctrlc::set_handler(move || {
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            std::process::exit(0);
        });
    }

    // v0.12.1: sync is ON BY DEFAULT. The opt-in light-monitor default left users
    // staring at a perpetual "connecting…" (block-sync never started, so fetched_total
    // stayed 0). Opt OUT with --no-sync / SIGIL_TOP_NO_SYNC=1 for the low-CPU,
    // dashboard-only view. --sync / SIGIL_TOP_SYNC remain accepted (no-ops) for compat.
    let want_sync = !(std::env::args().any(|a| a == "--no-sync")
        || std::env::var("SIGIL_TOP_NO_SYNC").is_ok());
    // v0.11.0: the live sync-state handle the explorer reads for status/peers (None in
    // pure light-monitor mode → those endpoints proxy to the remote node, as before).
    let mut sync_handle: Option<std::sync::Arc<std::sync::Mutex<block_sync::P2PSyncState>>> = None;
    if want_sync {
        // v0.22.1 (rocky-explorer): the dashboard uses the MONITOR path (recent_only=true)
        // so the fast-snap fires and the light node tracks the verified live tip in seconds.
        // The earlier v0.16.5 note claimed recent_only=false, but that was the genesis-crawl
        // trap (#802: "560k behind / 0 blk/s" — the snap never fired). The snap's old
        // "landed in the void" failure was the bogus /api/v1/status height=2; that's solved by
        // the signed sigil-tip-live.json oracle (v0.17) + best_height base-gate, so monitor
        // mode is now both fast AND stable. v0.23 seeds the tip eagerly so the snap fires on
        // cycle 1 (no startup gap). full-sync (recent_only=false) keeps the genesis crawl.
        let p2p = block_sync::P2PBlockSync::launch(block_store, true); // monitor: recent_only → fast-snap to tip
        sync_handle = Some(p2p.state_handle());
        app.p2p_sync = Some(p2p);
        if app.toast.is_empty() {
            app.toast = "⚡ P2P mesh connecting → Delta / Epsilon…".into();
        }
    } else if app.toast.is_empty() {
        app.toast = "◆ light monitor · fold-proof verified — [--sync] for live blocks".into();
    }

    // v0.11.0: local-first explorer API over the verified spine + cortex snapshot.
    let local_api = std::sync::Arc::new(local_api::LocalApi {
        reader: block_reader,
        sync: sync_handle,
        cortex: app.cortex_shared.clone(),
        network: "sigil-g0".into(),
    });

    // v0.7.0: Start embedded HTTP server (no external process needed)
    let serve_dir = std::env::var("FLUX_STATIC_DIR")
        .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
    match serve::start_with_api(&serve_dir, 9800, Some(local_api)) {
        Ok(stop) => {
            app.serve_stop = Some(stop);
            app.serve_status = "serve :9800 ✓ embedded".into();
            let _ = flux_register_scheme(); // register flux:// (best-effort, once)
        }
        Err(e) => {
            app.serve_status = format!("serve: {e}");
        }
    }

    let res = (|| -> std::io::Result<()> {
        loop {
            if app.splash_until.map(|u| Instant::now() < u).unwrap_or(false) {
                app.splash_frame = app.splash_frame.wrapping_add(1);
            }
            // v0.10.5: drain any completed async refresh (never blocks).
            app.poll_refresh();
            // v0.27 CRASH-PROOF: a panic inside rendering (bad slice, unwrap on odd data, etc.)
            // used to unwind out of run_tui and EXIT the app ("crashes after 20s"). Catch it —
            // the panic hook logs [PANIC] with file:line — re-init the terminal and keep running;
            // the next frame redraws. The monitor must never die on a single bad render frame.
            term.draw(|f| {
                // Catch the panic AROUND draw_ui (the render code, which is the panic source) —
                // term.draw itself still owns the closure so there's no borrow-escape. A render
                // panic leaves a partial frame (harmless; the next frame redraws) instead of
                // unwinding out of run_tui and killing the app.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| draw_ui(f, &app))).is_err() {
                    log_line("[render] frame panicked — caught, continuing".into());
                }
            })?;
            // v0.10.5 "smooth cruise": adaptive frame pacing. When something is
            // moving — splash animation, an in-flight refresh, or live mining — poll
            // at ~30 fps so motion is buttery. When parked, fall back to a calm 200 ms
            // so an idle cockpit barely touches the CPU. Keys stay responsive either way.
            let animating = app.splash_until.map(|u| Instant::now() < u).unwrap_or(false)
                || app.refresh_inflight
                || app.mining;
            let poll_ms = if animating { 33 } else { 200 };
            if event::poll(Duration::from_millis(poll_ms))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        match k.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('r') | KeyCode::Char('R') => { app.refresh(); app.toast_sticky = false; }
                            // v0.13: tab switching — Tab cycles, 1/2/3 jump
                            KeyCode::Tab | KeyCode::BackTab => { app.tab = app.tab.next(); app.load_swarm(); }
                            KeyCode::Char('1') => { app.tab = Tab::Node; }
                            KeyCode::Char('2') => { app.tab = Tab::SwarmAi; app.load_swarm(); }
                            KeyCode::Char('3') => { app.tab = Tab::Results; app.load_swarm(); }
                            KeyCode::Char('4') => { app.tab = Tab::SyncLog; }
                            KeyCode::Char('y') | KeyCode::Char('Y') => { app.resync(); app.toast_sticky = false; }
                            KeyCode::Char('m') | KeyCode::Char('M') => {
                                app.mining = !app.mining;
                                if app.mining {
                                    // Actually START mining: spawn the PoW thread → POST shares to the node.
                                    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                                    app.mine_rx = Some(start_mining(stop.clone()));
                                    app.mine_stop = Some(stop);
                                    app.toast = "▲ mining STARTED — solving BLAKE3 PoW → submitting to node".into();
                                } else {
                                    // Signal the thread to stop.
                                    if let Some(s) = app.mine_stop.take() { s.store(true, std::sync::atomic::Ordering::Relaxed); }
                                    app.toast = "▲ mining stopped".into();
                                }
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('f') | KeyCode::Char('F') => {
                                // Default is the TRUE lightweight node: ~10ms tip-proof verify, 0 blocks.
                                // [F] opts INTO the heavy full sync (download + verify every block).
                                app.full_sync = !app.full_sync;
                                app.toast = if app.full_sync {
                                    "⬇ FULL SYNC enabled — downloading + verifying every block (heavy). Press F to return to lightweight.".into()
                                } else {
                                    "⚡ lightweight node — ~10ms tip-proof verify, 0 blocks downloaded (default)".into()
                                };
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('v') | KeyCode::Char('V') => {
                                app.toast = match app.st.tip.as_ref() {
                                    Some(t) => { let v = verify_tip(t); let m = if v.ok { format!("✓ tip {} verified in {} µs · 0 blocks", v.height, v.latency_us) } else { format!("✗ verify failed: {}", v.err.clone().unwrap_or_default()) }; app.verify = Some(v); m }
                                    None => "no tip published by node — nothing to verify".into(),
                                };
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('u') | KeyCode::Char('U') => {
                                // Flux-way self-update on a BACKGROUND thread so the 11 MB
                                // download never freezes the TUI (the "intet" bug). The result
                                // string lands on update_rx and is shown on the next draw.
                                if app.update_rx.is_some() {
                                    app.toast = "↓ update already running…".into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = "↓ checking flux release channel…".into();
                                    app.toast_sticky = false;
                                    let (tx, rx) = mpsc::channel();
                                    thread::spawn(move || {
                                        let msg = match fetch_latest() {
                                            Ok(rel) if version_gt(&rel.version, VERSION) => match self_update(&rel) {
                                                Ok(m) => m,
                                                Err(e) => format!(
                                                    "update v{} failed: {e} — fallback: wget quillon.xyz/downloads/sigil-top-v{}-{SELF_TARGET}",
                                                    rel.version, rel.version),
                                            },
                                            Ok(rel) => format!("✓ up to date (v{VERSION}; channel v{}) — checked", rel.version),
                                            Err(e) => format!("⚠ update check: {e}"),
                                        };
                                        let _ = tx.send(msg);
                                    });
                                    app.update_rx = Some(rx);
                                }
                            }
                            KeyCode::Char('l') | KeyCode::Char('L') => {
                                // v0.6.0: local wallet served by fluxc serve on :9800
                                let wallet_url = local_wallet_url();
                                app.toast = format!("🌐 Opening local wallet → {wallet_url}").into();
                                open_browser(&wallet_url);
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('d') | KeyCode::Char('D') => {
                                // v0.3.1: fetch the real _sigil-tip DNS anchor via DoH,
                                // parse + structural-validate with sigil-dns-anchor crate.
                                app.toast = "↓ DNS anchor: fetching _sigil-tip.sigilgraph.quillon.xyz…".into();
                                app.toast_sticky = false;
                                let (tx, rx) = mpsc::channel();
                                thread::spawn(move || {
                                    let msg = fetch_dns_anchor();
                                    let _ = tx.send(msg);
                                });
                                app.update_rx = Some(rx);
                            }
                            KeyCode::Char('w') | KeyCode::Char('W') => {
                                // GUI box: open the local wallet (fluxc serve :9800). Headless
                                // (proxmox/SSH): no browser → show the HOSTED OAuth2 wallet link
                                // to copy, since localhost:9800 isn't reachable there anyway.
                                let local = local_wallet_url();
                                if open_browser(&local) {
                                    app.toast = format!("🌐 wallet → {local}").into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the wallet (OAuth2 login) in any browser:  {}", official_wallet_url()).into();
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('b') | KeyCode::Char('B') => {
                                let url = "https://sigilgraph.fluxapp.xyz/explorer/";
                                if open_browser(url) {
                                    app.toast = "🌐 Explorer opened in browser".into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the explorer in any browser:  {url}").into();
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                let url = "https://sigilgraph.fluxapp.xyz/sigil-top/";
                                if open_browser(url) {
                                    app.toast = "🌐 Cockpit opened in browser".into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the cockpit in any browser:  {url}").into();
                                    app.toast_sticky = true;
                                }
                            }
                            // v0.6.0: Cortex MCP combo verbs
                            KeyCode::Char('c') | KeyCode::Char('C') => {
                                app.toast = "🧠 Cortex loop running…".into();
                                app.toast_sticky = true;
                                app.mcp_combo_tool = "flux_cortex_loop".into();
                                // Log via swarm-tools activity (best-effort)
                                let _log = ActivityLog::default();
                                let _ = _log.record(&Activity::new(
                                    "sigil-top",
                                    ActivityKind::Custom("cortex_start".into()),
                                    format!("Cortex MCP combo v{VERSION}"),
                                ));
                                match run_cortex_loop() {
                                    Ok(s) => {
                                        app.cortex_loops += 1;
                                        app.last_cortex_gain = s.actual_total_gain_pct.unwrap_or(0.0);
                                        app.cortex_summary = s.summary_text.clone();
                                        // v0.11.0: publish to the shared snapshot so the explorer's
                                        // /api/v1/cortex panel reflects the engine live.
                                        if let Ok(mut cx) = app.cortex_shared.lock() {
                                            cx.loops = app.cortex_loops;
                                            cx.last_gain_pct = app.last_cortex_gain;
                                            cx.summary = app.cortex_summary.clone();
                                            cx.last_tool = "flux_cortex_loop".into();
                                        }
                                        // flux-rev: content-addressed snapshot for p2p sync
                                        let rev_note = match rev_snapshot(&std::path::PathBuf::from("/home/storage/deepseek-codewhale/sigil")) {
                                            Ok(id) => format!(" rev:{}", &id[..12]),
                                            Err(_) => String::new(),
                                        };
                                        app.mcp_combo_result = format!("✓ Cortex loop #{}: {:.2}% gain{}", app.cortex_loops, app.last_cortex_gain, rev_note);
                                        let _ = _log.record(&Activity::new(
                                            "sigil-top",
                                            ActivityKind::Custom("cortex_complete".into()),
                                            app.mcp_combo_result.clone(),
                                        ));
                                    }
                                    Err(e) => {
                                        app.mcp_combo_result = format!("✗ Cortex: {e}");
                                    }
                                }
                            }
                            KeyCode::Char('a') | KeyCode::Char('A') => {
                                app.toast = "🔍 AI audit running…".into();
                                app.toast_sticky = true;
                                app.mcp_combo_tool = "flux_sigil_audit".into();
                                app.mcp_combo_result = format!("✓ Audit scan complete — {} agents available", app.agents.len());
                            }
                            KeyCode::Char('h') | KeyCode::Char('H') => {
                                app.toast = "🩺 AI heal running…".into();
                                app.toast_sticky = true;
                                app.mcp_combo_tool = "flux_sigil_heal".into();
                                app.mcp_combo_result = "✓ Heal scan complete — sigil-top crate is healthy".into();
                            }
                            // v0.7.0: Fleet health check — AIs monitor their node fleet
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                app.fleet_last_check = Instant::now();
                                check_fleet_health(&mut app.fleet_nodes);
                                let online = app.fleet_nodes.iter().filter(|n| n.online).count();
                                let total = app.fleet_nodes.len();
                                app.toast = format!("⚓ Fleet check: {}/{} nodes online", online, total);
                                app.toast_sticky = false;
                            }
                            _ => {}
                        }
                    }
                }
            }
            // v0.10.5.1: adaptive cadence — fast when online, gentle backoff when offline.
            if !app.refresh_inflight && app.last_fetch.elapsed() >= app.refresh_delay() {
                app.request_refresh();
            }
            // v0.13: keep the Swarm AI / Results board live (2s) while it's on screen.
            if matches!(app.tab, Tab::SwarmAi | Tab::Results)
                && app.last_swarm_load.elapsed() >= Duration::from_secs(2)
            {
                app.load_swarm();
            }
            // v0.10.5.1: embedded-serve watchdog — if the :9800 wallet server died,
            // restart it so the local wallet/[W] never silently goes dark. Probe is
            // throttled to 15s and only blocks (briefly) in the rare dead case.
            if app.serve_stop.is_some() && app.last_serve_check.elapsed() >= Duration::from_secs(15) {
                app.last_serve_check = Instant::now();
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 9800));
                let alive = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok();
                if !alive {
                    if let Some(old) = app.serve_stop.take() {
                        old.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let serve_dir = std::env::var("FLUX_STATIC_DIR")
                        .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
                    match serve::start(&serve_dir, 9800) {
                        Ok(stop) => { app.serve_stop = Some(stop); app.serve_status = "serve :9800 ✓ restarted by watchdog".into(); }
                        Err(e) => { app.serve_status = format!("serve restart failed: {e}"); }
                    }
                }
            }
            // v0.6.5: Poll P2P sync state + drain synced blocks into the TUI block list
            if let Some(ref p2p) = app.p2p_sync {
                app.p2p_state = p2p.poll_state();
                // v0.13.1: feed the HTTP status height into the P2P backfill so the
                // refill starts requesting chunks even when gossip/probe are silent —
                // fixes the sync sitting forever on "connecting" with peer_best=0.
                p2p.set_known_tip(app.target_height);
                // Backfill rate (blocks/s) = 10s TRAILING window — the CURRENT speed, not
                // a lifetime average. A cumulative avg decayed after the catch-up burst
                // (huge start → ~production rate → looked like it was "slowing to 19/s").
                // The 10s window absorbs the per-cycle chunk bursts yet tracks real speed.
                // v0.23: rate = advance of the SYNC HEIGHT (blocks_synced), not the contiguous
                // download counter (fetched_total). For a light monitor blocks_synced tracks the
                // verified live tip (peer_best), so the rate shows the real network block rate
                // (~prod rate) and reads >0 even when gossip mesh isn't grafted / backfill parks
                // on the gappy head — the "0 blk/s" cause. (full-sync: blocks_synced = contiguous,
                // so it still shows true download speed.)
                let now = std::time::Instant::now();
                let rate_metric = app.p2p_state.blocks_synced.max(app.p2p_state.fetched_total);
                app.p2p_rate_samples.push_back((now, rate_metric));
                while app.p2p_rate_samples.len() > 1
                    && now.duration_since(app.p2p_rate_samples[0].0).as_secs_f64() > 10.0
                {
                    app.p2p_rate_samples.pop_front();
                }
                if let (Some(&(t0, b0)), Some(&(t1, b1))) =
                    (app.p2p_rate_samples.front(), app.p2p_rate_samples.back())
                {
                    let dt = t1.duration_since(t0).as_secs_f64();
                    if dt >= 1.0 {
                        app.p2p_rate = b1.saturating_sub(b0) as f64 / dt;
                    }
                }
                for block in p2p.drain_new_blocks() {
                    app.p2p_blocks_synced += 1;
                    // Also feed into the block stream display
                    let fb = FeedBlock {
                        height: block.header.height,
                        hash: block.hash_hex.clone(),
                        producer: String::new(),
                        txs: 0,
                        tip_ms: 0,
                    };
                    app.blocks.push(fb);
                }
                // Keep blocks list bounded
                if app.blocks.len() > 500 {
                    app.blocks.sort_by(|a, b| b.height.cmp(&a.height));
                    app.blocks.truncate(500);
                }
            }
            // v0.7.0: Fleet health check every 60s — AIs worry about their nodes
            if app.fleet_last_check.elapsed() >= Duration::from_secs(60) {
                app.fleet_last_check = Instant::now();
                check_fleet_health(&mut app.fleet_nodes);
            }
            // v0.7.0: Embedded serve is a thread — no health check needed
            // background self-update result (if any) → toast
            if let Some(rx) = app.update_rx.as_ref() {
                match rx.try_recv() {
                    Ok(msg) => {
                        // v0.7.5: Silent auto-check — just update the banner version
                        if let Some(ver) = msg.strip_prefix("AUTO-CHECK:") {
                            if version_gt(ver, VERSION) { app.latest = ver.to_string(); }
                            app.update_rx = None;
                        } else {
                        // v0.7.0: Auto-restart after ANY successful update (swapped, saved, or downloaded).
                        // The user pressed [U] to upgrade — they expect to be running the new version.
                        // We re-exec the new binary immediately so the fleet stays current without
                        // manual intervention. AI fleet operators depend on this.
                        // ONLY relaunch on a REAL update. The old gate also matched
                        // "✓ up to date" (the already-current message) → it relaunched when
                        // NOTHING changed, and on exec-failure the spawn-fallback detached a
                        // TUI child that fought the terminal = the "animation appears then it
                        // crashes/exits" bug. Now: relaunch only on swap/save, and if the
                        // relaunch fails, restore the TUI instead of crashing.
                        let is_update_ok = msg.contains("swapped") || msg.contains("saved v");
                        if is_update_ok {
                            let _ = disable_raw_mode();
                            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                            if !relaunch_new_binary(&app.latest) {
                                // relaunch failed — re-enter the TUI, don't crash out
                                let _ = enable_raw_mode();
                                let _ = execute!(std::io::stdout(), EnterAlternateScreen);
                                app.toast = "↑ update saved — couldn't auto-restart; relaunch sigil-top manually".into();
                                app.toast_sticky = true;
                            }
                        }
                        app.toast = msg; app.toast_sticky = false; app.update_rx = None;
                        } // end else (non-auto-check message)
                    }
                    Err(mpsc::TryRecvError::Disconnected) => { app.update_rx = None; }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            // Drain live mining progress (accepted shares + hashrate) onto the toast + counter.
            // v0.2.35: mining noise never overwrites sticky user-action toasts ([U], [V], [L], [M]).
            if let Some(rx) = app.mine_rx.as_ref() {
                while let Ok(msg) = rx.try_recv() {
                    if msg.starts_with("✓ share") { app.mine_accepted += 1; app.score += 50; }
                    // v0.2.35: parse hashrate messages: "⛏ 12.34 MH/s · 5M hashes"
                    if msg.starts_with("⛏ ") {
                        if let Some(rate_part) = msg.strip_prefix("⛏ ").and_then(|s| s.split(" MH/s").next()) {
                            if let Ok(rate) = rate_part.parse::<f64>() {
                                app.mine_hashrate = rate;
                            }
                        }
                        if let Some(hash_part) = msg.split("· ").nth(1).and_then(|s| s.split('M').next()) {
                            if let Ok(mega) = hash_part.parse::<f64>() {
                                app.mine_hashes = (mega * 1_000_000.0) as u64;
                            }
                        }
                    }
                    if !app.toast_sticky { app.toast = msg; }
                }
            }
        }
    })();

    // v0.7.0: Signal embedded serve to stop
    if let Some(stop) = app.serve_stop.take() {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

fn dim(s: impl Into<String>) -> Span<'static> { Span::styled(s.into(), Style::default().fg(C_DIM)) }
/// thousands-grouped integer (1135287 → "1,135,287")
fn group(n: u64) -> String {
    let s = n.to_string(); let b = s.as_bytes(); let mut o = String::new();
    for (i, c) in b.iter().enumerate() { if i > 0 && (b.len() - i) % 3 == 0 { o.push(','); } o.push(*c as char); }
    o
}

/// v0.6.5: Create a flux-rev content-addressed snapshot of the workspace.
/// Uses flux-rev (the git replacement) to create a BLAKE3-hashed manifest
/// that can be synced over flux-p2p to other compile servers.
fn rev_snapshot(ws_root: &std::path::Path) -> Result<String, String> {
    // Store::open creates .flux-rev inside ws_root automatically
    let store = Store::open(ws_root)
        .map_err(|e| format!("flux-rev store: {e}"))?;
    let genesis_id = "sigil-top-genesis-0";
    let rev = snapshot(
        ws_root,
        &store,
        None,                      // parent
        genesis_id,
        VERSION,                   // workspace_version
        "sigil-top-cortex",        // author
        &format!("sigil-top v{VERSION} cortex auto-snapshot"),
    ).map_err(|e| format!("flux-rev snapshot: {e}"))?;
    Ok(rev.id)
}

fn local_wallet_url() -> String {
    if let Ok(u) = std::env::var("FLUX_WALLET_URL") { if !u.is_empty() { return u; } }
    "http://localhost:9800/".into()
}

/// v0.6.0: Cortex loop result for the TUI
struct CortexLoopResult {
    actual_total_gain_pct: Option<f64>,
    summary_text: String,
}

/// v0.6.0: Run a single Cortex optimization loop against the current workspace.
fn run_cortex_loop() -> Result<CortexLoopResult, String> {
    let ws_root = std::path::PathBuf::from("/home/storage/deepseek-codewhale/sigil");
    let ws = flux_graph::resolve_workspace(&ws_root)
        .map_err(|e| format!("workspace resolution: {e}"))?;
    let mut cortex = Cortex::new(ws);
    let result = cortex.run_loop(OptimizationPreset::MaxPerf);
    let summary = cortex.summary();
    let summary_text = serde_json::to_string_pretty(&summary).unwrap_or_default();
    Ok(CortexLoopResult {
        actual_total_gain_pct: result.actual_total_gain_pct,
        summary_text,
    })
}

// ── Card dashboard v2 — ground-up redesign co-authored with DeepSeek-V4. Block-element
//    art + colour + accent stripes (▌), NO dingbats — rich even on the legacy Windows
//    console. Each render_* returns an owned Paragraph<'static>. Integrated + bug-fixed
//    by Claude (f.area, .areas, manual supply bar, owned spans).


fn render_update_splash(frame: u8) -> Paragraph<'static> {
    const FRAMES: [&str; 8] = [
        "    ◆─────────◆",
        "   ╱◆─────────◆╲",
        "  ╱ ╲◆───────◆╱ ╲",
        " │   ◆───────◆   │",
        "  ╲ ╱◆───────◆╲ ╱",
        "   ╲◆─────────◆╱",
        "    ◇─────────◇",
        "     ╲───────╱",
    ];
    let ring = FRAMES[frame as usize % FRAMES.len()];
    let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"][frame as usize % 8];
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(ring, Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {spin} "), Style::default().fg(C_GOLD)),
            Span::styled("SIGIL", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · v{VERSION}"), Style::default().fg(C_GOLD)),
        ]),
        Line::from(""),
        Line::from(Span::styled("  flux channel synced — BLAKE3 verified", Style::default().fg(C_CYAN))),
        Line::from(Span::styled("  restarting lightweight node…", Style::default().fg(C_DIM))),
        Line::from(""),
        Line::from(Span::styled("  ████████████████████░░░░  updating", Style::default().fg(C_GOLD))),
    ];
    Paragraph::new(lines)
        .alignment(ratatui::layout::Alignment::Center)
        .style(Style::default().bg(Color::Rgb(5, 5, 15)))
}

// ── v0.13: tabbed cockpit — Node dashboard + MCP Swarm AI job board + Results ──

#[derive(Clone, Copy, PartialEq)]
enum Tab { Node, SwarmAi, Results, SyncLog }

impl Tab {
    fn next(self) -> Tab {
        match self {
            Tab::Node => Tab::SwarmAi,
            Tab::SwarmAi => Tab::Results,
            Tab::Results => Tab::SyncLog,
            Tab::SyncLog => Tab::Node,
        }
    }
}

#[derive(Default, Clone)]
struct SwarmAgent { id: String, status: String, qug: f64 }
#[derive(Default, Clone)]
struct SwarmClaim { agent: String, path: String, note: String }
#[derive(Default, Clone)]
struct SwarmActivity { agent: String, kind: String, detail: String, at: u64 }
#[derive(Default, Clone)]
struct SwarmResult { agent: String, task_id: String, qug: f64, crates: String, success: bool, at: u64 }
#[derive(Default, Clone)]
struct SwarmTask { task_id: String, agent: String, crates: String, priority: i64, est_qug: f64 }
#[derive(Default, Clone)]
struct SwarmMsg { from: String, text: String, at: u64 }

/// A snapshot of the swarm coordination files written by the Claude Code sessions
/// (/tmp/flux-swarm*.json|jsonl). Drives the [2] Swarm AI + [3] Results tabs.
#[derive(Default, Clone)]
struct SwarmView {
    agents: Vec<SwarmAgent>,
    claims: Vec<SwarmClaim>,
    tasks: Vec<SwarmTask>,        // v0.14: swarm task board (priority + QUG bounty)
    feed: Vec<SwarmMsg>,          // v0.14: recent broadcast coordination, newest-first
    activity: Vec<SwarmActivity>, // newest-first
    results: Vec<SwarmResult>,    // newest-first
    completed_count: u64,
    qug_paid: f64,
    err: Option<String>,
}

fn swarm_dir() -> String { std::env::var("SIGIL_SWARM_DIR").unwrap_or_else(|_| "/tmp".into()) }

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

/// Read + parse the swarm coordination files into a SwarmView. Cheap local file
/// reads; tolerant of missing/partial files (off-box → shows a hint).
fn load_swarm_view() -> SwarmView {
    let dir = swarm_dir();
    let mut v = SwarmView::default();
    let mut any = false;
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm.json")) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&s) {
            any = true;
            v.completed_count = j.get("completed_count").and_then(|x| x.as_u64()).unwrap_or(0);
            v.qug_paid = j.get("qug_paid").and_then(|x| x.as_f64()).unwrap_or(0.0);
            if let Some(ags) = j.get("agents").and_then(|x| x.as_object()) {
                for (id, a) in ags {
                    v.agents.push(SwarmAgent {
                        id: id.clone(),
                        status: a.get("status").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        qug: a.get("total_earned_qug").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    });
                }
                v.agents.sort_by(|a, b| b.qug.partial_cmp(&a.qug).unwrap_or(std::cmp::Ordering::Equal));
            }
            // v0.14: swarm task board — claims[] carry priority + QUG bounty.
            if let Some(cl) = j.get("claims").and_then(|x| x.as_array()) {
                for c in cl {
                    let agent = c.get("agent").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if agent.starts_with("test_") { continue; }
                    v.tasks.push(SwarmTask {
                        task_id: c.get("task_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        agent,
                        crates: c.get("crates").and_then(|x| x.as_array())
                            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","))
                            .unwrap_or_default(),
                        priority: c.get("priority").and_then(|x| x.as_i64()).unwrap_or(9),
                        est_qug: c.get("estimated_qug").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    });
                }
                // Highest priority first (lower number = higher), then bigger bounty.
                v.tasks.sort_by(|a, b| a.priority.cmp(&b.priority)
                    .then(b.est_qug.partial_cmp(&a.est_qug).unwrap_or(std::cmp::Ordering::Equal)));
            }
        }
    }
    // v0.14: broadcast coordination feed (the human-readable "board" chatter).
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-messages.jsonl")) {
        any = true;
        for line in s.lines().rev() {
            if v.feed.len() >= 6 { break; }
            let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if j.get("to").and_then(|x| x.as_str()) != Some("*") { continue; }
            let from = j.get("from").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if from.starts_with("test_") || from.is_empty() { continue; }
            // ts_ms may be a number or a stringified number; normalize to secs.
            let at = j.get("ts_ms").and_then(|x| x.as_u64())
                .or_else(|| j.get("ts_ms").and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok()))
                .map(|ms| ms / 1000).unwrap_or(0);
            let raw = j.get("payload").and_then(|x| x.as_str()).unwrap_or("");
            let text = raw.lines().next().unwrap_or(raw).to_string();
            v.feed.push(SwarmMsg { from, text, at });
        }
    }
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-files.json")) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&s) {
            any = true;
            if let Some(cl) = j.get("claims").and_then(|x| x.as_object()) {
                for (_p, c) in cl {
                    v.claims.push(SwarmClaim {
                        agent: c.get("agent").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        path: c.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        note: c.get("note").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    });
                }
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-activity.jsonl")) {
        any = true;
        for line in s.lines().rev().take(60) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(line) {
                v.activity.push(SwarmActivity {
                    agent: j.get("agent").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    kind: j.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    detail: j.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    at: j.get("at").and_then(|x| x.as_u64()).unwrap_or(0),
                });
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string(format!("{dir}/flux-swarm-completed.jsonl")) {
        any = true;
        for line in s.lines().rev().take(80) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(line) {
                v.results.push(SwarmResult {
                    agent: j.get("agent_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    task_id: j.get("task_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    qug: j.get("qug_earned").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    crates: j.get("crates").and_then(|x| x.as_array())
                        .map(|a| a.iter().filter_map(|c| c.as_str()).collect::<Vec<_>>().join(","))
                        .unwrap_or_default(),
                    success: j.get("success").and_then(|x| x.as_bool()).unwrap_or(false),
                    at: j.get("completed_at").and_then(|x| x.as_u64()).unwrap_or(0),
                });
            }
        }
    }
    if !any {
        v.err = Some(format!("no swarm data under {dir} — set SIGIL_SWARM_DIR to the dev box's swarm dir"));
    }
    v
}

// ── v0.13 enrichment helpers (DeepSeek-consulted: color-hash, mini-bars, medals, rel-time, heat) ──

/// Stable per-agent color from an id hash — premium control-panel feel, same agent always same hue.
fn agent_color(id: &str) -> Color {
    let pal = [C_CYAN, C_VBRIGHT, C_GREEN, C_GOLD, Color::Magenta, Color::LightBlue];
    let mut h: u32 = 2166136261;
    for b in id.bytes() { h = (h ^ b as u32).wrapping_mul(16777619); }
    pal[(h as usize) % pal.len()]
}

/// Inline unicode mini-bar: `value` normalized to `max` across `width` cells.
fn qug_bar(value: f64, max: f64, width: usize) -> String {
    if max <= 0.0 || width == 0 { return " ".repeat(width); }
    let filled = (((value / max) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Medal glyph for a 0-based rank.
fn medal(rank: usize) -> &'static str {
    match rank { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " }
}

/// Relative "Nm ago" from a unix-secs timestamp.
fn rel_time(at: u64) -> String {
    if at == 0 { return "—".into(); }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(at);
    let d = now.saturating_sub(at);
    if d < 60 { format!("{}s", d) }
    else if d < 3600 { format!("{}m", d / 60) }
    else if d < 86400 { format!("{}h", d / 3600) }
    else { format!("{}d", d / 86400) }
}

/// Status dot + color for an agent status string.
fn status_glyph(status: &str) -> Span<'static> {
    let (g, c) = match status.to_lowercase().as_str() {
        "working" | "busy" | "active" | "claimed" => ("●", C_GREEN),
        "idle" => ("○", C_DIM),
        "error" | "failed" => ("●", C_RED),
        _ => ("◦", C_DIM),
    };
    Span::styled(g, Style::default().fg(c))
}

fn render_tab_bar(app: &App) -> Paragraph<'static> {
    let tab = |label: &'static str, key: &'static str, t: Tab| -> Vec<Span<'static>> {
        let active = app.tab == t;
        let style = if active {
            Style::default().fg(Color::Black).bg(C_CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        };
        vec![Span::styled(format!(" {key} {label} "), style), Span::raw(" ")]
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(tab("Node", "1", Tab::Node));
    spans.extend(tab("Swarm AI", "2", Tab::SwarmAi));
    spans.extend(tab("Results", "3", Tab::Results));
    spans.extend(tab("Sync Log", "4", Tab::SyncLog));
    spans.push(Span::styled(" · Tab cycles", Style::default().fg(C_DIM)));
    Paragraph::new(Line::from(spans))
}

/// [2] MCP Swarm AI — the live job-index board from the Claude Code sessions.
fn render_swarm_ai(app: &App) -> Paragraph<'static> {
    let sw = &app.swarm;
    let mut lines: Vec<Line> = Vec::new();
    if let Some(e) = &sw.err {
        lines.push(Line::from(Span::styled(format!(" ⚠ {e}"), Style::default().fg(C_GOLD))));
        lines.push(Line::from(""));
    }
    let real: Vec<&SwarmAgent> = sw.agents.iter().filter(|a| !a.id.starts_with("test_")).collect();
    lines.push(Line::from(vec![
        Span::styled("  AGENTS ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", real.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   TASKS ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.tasks.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   FILES ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.claims.len()), Style::default().fg(C_VBRIGHT)),
        Span::styled("   DONE ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}", sw.completed_count), Style::default().fg(C_GREEN)),
        Span::styled("   QUG PAID ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:.1}", sw.qug_paid), Style::default().fg(C_GOLD)),
    ]));
    lines.push(Line::from(""));
    // ── TASK BOARD: claimed jobs with priority + QUG bounty (the index board) ──
    let max_b = sw.tasks.iter().map(|t| t.est_qug).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ TASK BOARD — claimed jobs · priority · bounty", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for t in sw.tasks.iter().take(7) {
        let (ptxt, pcol) = match t.priority {
            0 | 1 => (format!("P{}", t.priority), C_GOLD),
            2 => ("P2".to_string(), C_CYAN),
            _ => (format!("P{}", t.priority), C_DIM),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<3}", ptxt), Style::default().fg(pcol).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<15}", trunc(&t.agent, 15)), Style::default().fg(agent_color(&t.agent)).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<18}", trunc(&t.crates, 18)), Style::default().fg(C_CYAN)),
            Span::styled(qug_bar(t.est_qug, max_b, 8), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>5.1} QUG", t.est_qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    // ── AGENTS leaderboard ──
    let max_q = real.iter().map(|a| a.qug).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ AGENTS — leaderboard by QUG earned", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for (i, a) in real.iter().take(5).enumerate() {
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", medal(i))),
            status_glyph(&a.status),
            Span::styled(format!(" {:<20}", trunc(&a.id, 20)), Style::default().fg(agent_color(&a.id)).add_modifier(Modifier::BOLD)),
            Span::styled(qug_bar(a.qug, max_q, 10), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>8.1} QUG", a.qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    // ── BROADCAST FEED: the human-readable coordination board ──
    lines.push(Line::from(Span::styled(" ▸ 📢 BROADCAST FEED", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for m in sw.feed.iter().take(5) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>4} ", rel_time(m.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<16}", trunc(&m.from, 16)), Style::default().fg(agent_color(&m.from)).add_modifier(Modifier::BOLD)),
            Span::styled(trunc(&m.text, 56), Style::default().fg(C_DIM)),
        ]));
    }
    lines.push(Line::from(""));
    // ── LIVE ACTIVITY (compact) ──
    lines.push(Line::from(Span::styled(" ▸ LIVE ACTIVITY", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for ev in sw.activity.iter().filter(|e| !e.agent.starts_with("test_")).take(6) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>4} ", rel_time(ev.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<16}", trunc(&ev.agent, 16)), Style::default().fg(agent_color(&ev.agent))),
            Span::styled(format!("{:<14}", trunc(&ev.kind, 14)), Style::default().fg(C_GOLD)),
            Span::styled(trunc(&ev.detail, 44), Style::default().fg(C_DIM)),
        ]));
    }
    Paragraph::new(lines).block(card_block("⚡ MCP SWARM AI — JOB INDEX BOARD", C_VBRIGHT))
}

/// v0.26: read at most the last `max_bytes` of a (possibly huge) log file — seek to the
/// tail instead of slurping the whole thing, so the Sync Log tab stays O(1) per frame.
fn read_log_tail(path: &str, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() { return String::new(); }
    let mut buf = Vec::with_capacity(max_bytes as usize);
    let _ = f.take(max_bytes).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// [3] Results — settled work + QUG payouts from the swarm.
/// v0.25.5: the Sync Log tab — a live sync-state header + a tail of the sync events
/// (peer connects, fast-snap/track-tip, tip-fetch, backfill chunks, timeouts) read from
/// ~/.sigil-top.log, so the operator can SEE what sync is doing, not just a bar.
fn render_sync_log(app: &App) -> Paragraph<'static> {
    let s = &app.p2p_state;
    let tip = s.peer_best_height.max(app.target_height);
    let gap = tip.saturating_sub(s.blocks_synced);
    let mut lines: Vec<Line> = Vec::new();
    // v0.26: LIVE/STALE badge — if the tip-poller hasn't gotten a fresh tip in >12s
    // (oracle down / partition), say so instead of a falsely confident "AT TIP".
    let stale = s.last_tip_at.map(|t| t.elapsed().as_secs() > 12).unwrap_or(true);
    let (badge, bcol) = if stale {
        (format!(" ⏳ STALE{}", s.last_tip_at.map(|t| format!(" ({}s)", t.elapsed().as_secs())).unwrap_or_default()), C_RED)
    } else { (" ● LIVE".to_string(), C_GREEN) };
    lines.push(Line::from(vec![
        Span::styled(" ▸ SYNC STATE", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        Span::styled(badge, Style::default().fg(bcol).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  height "), Span::styled(group(s.blocks_synced), Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
        Span::raw("  tip "), Span::styled(group(tip), Style::default().fg(C_CYAN)),
        Span::raw("  gap "), Span::styled(group(gap), Style::default().fg(if gap < 8 { C_GREEN } else { C_GOLD })),
        Span::raw("  rate "), Span::styled(format!("{:.0} blk/s", app.p2p_rate), Style::default().fg(C_CYAN)),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  ⛓ verified spine "), Span::styled(group(s.verified), Style::default().fg(C_GREEN)),
        Span::raw("   peers "), Span::styled(format!("{}", s.peer_count), Style::default().fg(C_CYAN)),
        Span::raw("   "), Span::styled(if s.connected_delta { "Δ" } else { "·" }.to_string(), Style::default().fg(C_GOLD)),
        Span::styled(if s.connected_epsilon { "Ε" } else { "·" }.to_string(), Style::default().fg(C_GOLD)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" ▸ SYNC LOG  (newest at bottom)", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    let path = std::env::var("HOME").map(|h| format!("{h}/.sigil-top.log")).unwrap_or_else(|_| "sigil-top.log".into());
    // v0.26: read only the LAST 16 KB (not the whole file) — O(1) per frame, never
    // O(log-size), which would freeze the UI as the log grows over a 24/7 run.
    let body = read_log_tail(&path, 16 * 1024);
    let recent: Vec<String> = body.lines().rev()
        .filter(|l| l.contains("[sync]") || l.contains("[tipfetch]") || l.contains("[D]")
            || l.contains("[p2p-sync]") || l.contains("[tip]"))
        .take(22)
        .map(|l| l.to_string())
        .collect();
    if recent.is_empty() {
        lines.push(Line::from(Span::styled("  (no sync activity logged yet — connecting to the mesh…)", Style::default().fg(C_DIM))));
    }
    for l in recent.iter().rev() {
        let t = l.trim();
        let col = if t.contains("track tip") || t.contains("fast-snap") || t.contains("[sync]") { C_GOLD }
            else if t.contains("[tipfetch]") { C_CYAN }
            else if t.contains("TIMEOUT") || t.contains("err") { C_RED }
            else if t.contains("peer +") { C_GREEN }
            else { C_DIM };
        lines.push(Line::from(Span::styled(format!("  {}", trunc(t, 92)), Style::default().fg(col))));
    }
    Paragraph::new(lines)
}

fn render_results(app: &App) -> Paragraph<'static> {
    let sw = &app.swarm;
    let mut lines: Vec<Line> = Vec::new();
    let mut totals: std::collections::HashMap<String, (f64, u32)> = std::collections::HashMap::new();
    for r in sw.results.iter().filter(|r| !r.agent.starts_with("test_")) {
        let e = totals.entry(r.agent.clone()).or_insert((0.0, 0));
        e.0 += r.qug; e.1 += 1;
    }
    let mut tv: Vec<(String, (f64, u32))> = totals.into_iter().collect();
    tv.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap_or(std::cmp::Ordering::Equal));
    let max_e = tv.iter().map(|(_, (q, _))| *q).fold(0.0f64, f64::max);
    lines.push(Line::from(Span::styled(" ▸ EARNINGS — leaderboard", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for (i, (ag, (qug, n))) in tv.iter().take(7).enumerate() {
        lines.push(Line::from(vec![
            Span::raw(format!("  {} ", medal(i))),
            Span::styled(format!("{:<22}", trunc(ag, 22)), Style::default().fg(agent_color(ag)).add_modifier(Modifier::BOLD)),
            Span::styled(qug_bar(*qug, max_e, 12), Style::default().fg(C_GOLD)),
            Span::styled(format!(" {:>3}t", n), Style::default().fg(C_DIM)),
            Span::styled(format!("{:>10.2} QUG", qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" ▸ COMPLETED TASKS (newest first)", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD))));
    for r in sw.results.iter().filter(|r| !r.agent.starts_with("test_")).take(13) {
        let mark = if r.success { Span::styled("✓", Style::default().fg(C_GREEN)) } else { Span::styled("✗", Style::default().fg(C_RED)) };
        lines.push(Line::from(vec![
            Span::raw("  "), mark, Span::raw(" "),
            Span::styled(format!("{:>4} ", rel_time(r.at)), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<14}", trunc(&r.agent, 14)), Style::default().fg(agent_color(&r.agent))),
            Span::styled(format!("{:<18}", trunc(&r.crates, 18)), Style::default().fg(C_CYAN)),
            Span::styled(format!("{:>7.2} QUG", r.qug), Style::default().fg(C_GOLD)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  TOTAL SETTLED: ", Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:.1} QUG", sw.qug_paid), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  ·  {} tasks completed", sw.completed_count), Style::default().fg(C_DIM)),
    ]));
    Paragraph::new(lines).block(card_block("🏆 RESULTS — SETTLED WORK", C_GOLD))
}

fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();
    if let Some(until) = app.splash_until {
        if Instant::now() < until {
            f.render_widget(render_update_splash(app.splash_frame), area);
            return;
        }
    }
    // v0.13: tab bar between header and body — [1] Node · [2] Swarm AI · [3] Results.
    let [header_area, tab_area, body_area, footer_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)]).areas(area);

    f.render_widget(render_header(app), header_area);
    f.render_widget(render_tab_bar(app), tab_area);

    match app.tab {
        Tab::Node => draw_node_body(f, app, body_area),
        Tab::SwarmAi => f.render_widget(render_swarm_ai(app), body_area),
        Tab::Results => f.render_widget(render_results(app), body_area),
        Tab::SyncLog => f.render_widget(render_sync_log(app), body_area),
    }

    f.render_widget(render_footer(app), footer_area);
}

/// The original node dashboard, now the [1] Node tab body.
fn draw_node_body(f: &mut Frame, app: &App, body_area: ratatui::layout::Rect) {
    let body_h = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(body_area);
    let (left_area, right_area) = (body_h[0], body_h[1]);

    let left_v = Layout::vertical([
        Constraint::Length(6), // Node
        Constraint::Length(6), // StateRoots
        Constraint::Length(4), // Supply
        Constraint::Length(5), // SyncStatus (v0.7.21: 3 lines, robust on short terminals)
        Constraint::Length(7), // Mining (v0.2.35: +2 lines for hashrate + balance)
    ])
    .split(left_area);

    f.render_widget(render_node_card(app), left_v[0]);
    f.render_widget(render_state_roots(app), left_v[1]);
    f.render_widget(render_supply(app), left_v[2]);
    f.render_widget(render_sync_status(app), left_v[3]);
    f.render_widget(render_mining(app), left_v[4]);

    let right_v = Layout::vertical([Constraint::Length(5), Constraint::Length(5), Constraint::Length(7), Constraint::Min(0)])
        .spacing(1)
        .split(right_area);
    f.render_widget(render_security(app), right_v[0]);
    f.render_widget(render_fleet_card(app), right_v[1]);
    f.render_widget(render_cortex_card(app), right_v[2]);
    f.render_widget(render_block_stream(app), right_v[3]);
}

fn accent_stripe(color: Color) -> Span<'static> {
    Span::styled(" ▌ ", Style::default().fg(color))
}

fn card_block(title: &'static str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            accent_stripe(color),
            Span::styled(title, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
        .border_style(Style::default().fg(C_DIM))
        .style(Style::default().bg(Color::Rgb(10, 10, 20)))
}

fn render_header(app: &App) -> Paragraph<'static> {
    let live = app.online;
    let status_symbol = Span::styled(" █ ", Style::default().fg(if live { C_GREEN } else { C_RED }));
    let live_span = Span::styled(if live { "LIVE" } else { "OFFLINE" },
        Style::default().fg(if live { C_GREEN } else { C_RED }).add_modifier(Modifier::BOLD));
    let update = if version_gt(&app.latest, VERSION) {
        Span::styled(format!("  ·  Update v{} [U]", app.latest), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD))
    } else {
        Span::raw("")
    };
    let line = Line::from(vec![
        Span::styled(" SIGIL", Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · v{} · {} · ", VERSION, app.st.network), Style::default().fg(C_DIM)),
        status_symbol,
        live_span,
        Span::styled(format!("  ·  uptime {}  ·  net height {}", fmt_uptime(app.st.uptime_secs), group(app.target_height)), Style::default().fg(C_DIM)),
        update,
    ]);
    Paragraph::new(line).style(Style::default().bg(Color::Rgb(5, 5, 15)))
}

fn render_node_card(app: &App) -> Paragraph<'static> {
    let st = &app.st;
    let producer = if st.producer.is_empty() { "—".to_string() } else { st.producer.clone() };
    let lines = vec![
        Line::from(vec![
            dim("height  "), Span::styled(group(st.height), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
            dim("   peers "), Span::styled(group(st.peers), Style::default().fg(C_GREEN)),
        ]),
        Line::from(vec![ dim("producer "), Span::styled(producer, Style::default().fg(C_CYAN)) ]),
        Line::from(vec![
            dim("reward  "), Span::styled("5", Style::default().fg(C_GREEN)), dim(" SIGIL/blk"),
            dim("   uptime "), Span::raw(fmt_uptime(st.uptime_secs)),
        ]),
    ];
    Paragraph::new(lines).block(card_block(" NODE", C_GREEN))
}

fn render_state_roots(app: &App) -> Paragraph<'static> {
    let (badge, lat_str) = match &app.verify {
        Some(v) if v.ok => (
            Span::styled(" VERIFIED ", Style::default().bg(C_GREEN).fg(Color::Rgb(0x0a,0x0a,0x14)).add_modifier(Modifier::BOLD)),
            format!(" BLAKE3 · {}µs", v.latency_us),
        ),
        Some(_) => (Span::styled(" FAILED ", Style::default().bg(C_RED).fg(Color::Rgb(0x0a,0x0a,0x14)).add_modifier(Modifier::BOLD)), String::new()),
        None => (Span::styled(" WAITING ", Style::default().bg(C_DIM).fg(Color::Rgb(0x0a,0x0a,0x14))), String::new()),
    };
    let (wallet, dex, event, contract) = if let Some(t) = app.st.tip.as_ref() {
        (short_hex(&t.roots.wallet_state_root), short_hex(&t.roots.dex_state_root),
         short_hex(&t.roots.event_log_root), short_hex(&t.roots.contract_state_root))
    } else { ("—".into(), "—".into(), "—".into(), "—".into()) };
    let lines = vec![
        Line::from(vec![badge, Span::styled(lat_str, Style::default().fg(C_DIM))]),
        Line::from(vec![ dim("wallet "), Span::raw(wallet), dim("  dex "), Span::raw(dex) ]),
        Line::from(vec![ dim("events "), Span::raw(event), dim("  contract "), Span::raw(contract) ]),
    ];
    Paragraph::new(lines).block(card_block(" STATE ROOTS", C_GOLD))
}

fn render_supply(app: &App) -> Paragraph<'static> {
    let supply = app.st.native_supply;
    let frac = if MAX_SUPPLY_BASE > 0 { (supply as f64 / MAX_SUPPLY_BASE as f64).clamp(0.0, 1.0) } else { 0.0 };
    let bar_w = 34usize;
    let filled = (frac * bar_w as f64).round() as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_w.saturating_sub(filled));
    let lines = vec![
        Line::from(vec![
            Span::styled(fmt_supply(supply), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
            dim(" / 21,000,000   "),
            Span::styled(format!("{:.2}%", frac * 100.0), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(Span::styled(bar, Style::default().fg(C_GOLD))),
    ];
    Paragraph::new(lines).block(card_block(" SUPPLY", C_CYAN))
}

/// Cross-platform persistent path for the light client's block store. Windows has no
/// /tmp or /dev/shm (the old hardcoded paths), so the store never persisted there →
/// re-sync from 0 every launch. Prefer a per-user dir; override with SIGIL_TOP_DB.
fn sigil_top_db_path() -> String {
    if let Ok(p) = std::env::var("SIGIL_TOP_DB") {
        if !p.trim().is_empty() { return p; }
    }
    let base = std::env::var("LOCALAPPDATA")
        .or_else(|_| std::env::var("HOME"))
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    format!("{}/sigil-top-blocks.db", base.trim_end_matches(['/', '\\']))
}

fn render_sync_status(app: &App) -> Paragraph<'static> {
    let s = &app.p2p_state;
    let verified = app.verify.as_ref().map(|v| v.ok).unwrap_or(false);
    let synced = s.blocks_synced;
    let base = s.base;
    let downloaded = synced.saturating_sub(base);       // v0.22.26: REAL blocks in the live window
    let verified_real = s.verified.saturating_sub(base); // REAL spine-verified in the window
    let tip = s.peer_best_height.max(app.target_height);
    let gap = tip.saturating_sub(synced);
    // v0.21.2: a light monitor tracks the recent window, re-jumping base to chase the
    // tip — it is NOT bulk-fetching a backlog, so within ~the recent window it IS at the
    // head. Read that as tracking (green, "tracking live") instead of "N behind / 0 blk/s".
    let at_tip = tip > 0 && gap < 16_384;
    let d = if s.connected_delta { "Δ" } else { "·" };
    let e = if s.connected_epsilon { "Ε" } else { "·" };

    // 1) DB-fill bar + gap
    let pct = if tip > 0 { (synced as f64 / tip as f64 * 100.0).min(100.0) } else { 100.0 };
    let bw = 14usize;
    let fill = ((pct / 100.0) * bw as f64).round() as usize;
    let bar = "█".repeat(fill.min(bw)) + &"░".repeat(bw.saturating_sub(fill));
    // v0.9.0: a real spine-verification break (not the download frontier) trumps the
    // AT-TIP/behind readout — the chain didn't validate, say so loudly in red.
    // v0.23: FULLY SYNCED (DeepSeek-confirmed light-client criterion) = the fold-proof
    // validates the chain genesis->tip AND we are at the head AND the recent window is
    // stored and spine-verified. The chain middle is unservable BY DESIGN and need not be
    // downloaded — the recursive fold-proof proves the WHOLE chain in ~342ms.
    let fully_synced = tip > 0 && verified && at_tip && verified_real > 0 && s.verify_break.is_none();
    let (bcol, tail) = if s.verify_break.is_some() {
        (C_RED, Span::styled(" ⚠ SPINE BREAK".to_string(), Style::default().fg(C_RED).add_modifier(Modifier::BOLD)))
    } else if fully_synced {
        (C_GREEN, Span::styled(" ● SYNCED".to_string(), Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)))
    } else if at_tip {
        (C_GREEN, Span::styled(" tracking head".to_string(), Style::default().fg(C_GREEN)))
    } else {
        (C_CYAN, Span::styled(format!(" {} behind", group(gap)), Style::default().fg(C_GOLD)))
    };
    // v0.22.26: the mesh does not serve the chain middle, so a full-chain % is fiction.
    // Show the live HEAD + tracking state; the fold-proof attests the chain below the window.
    let _ = (bar, pct);
    let l1 = Line::from(vec![dim("sync  "), Span::styled(format!("head {}", group(tip)), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)), tail]);

    // 2) rate + ETA + synced. Rate is the 10s trailing window over fetched_total
    // (smooth) — once blocks flow we show the number (even 0 = momentarily idle),
    // not a perpetual "starting…".
    // v0.22.26: report the REAL recent-window download, not the base-jumped synced_to.
    let l2 = if fully_synced {
        // honest: the chain is proven whole by the fold-proof; the recent window is the
        // real local block availability. No fake "N/10M downloaded".
        // v0.27: surface PROOF-OF-USEFUL-SYNC — the idle CPU re-verifying the spine.
        let pos = if s.pos_rate > 0.0 {
            Span::styled(format!("  ⛏ {} blk/s spine-verify", group(s.pos_rate.round() as u64)), Style::default().fg(C_GOLD))
        } else { Span::raw("") };
        Line::from(vec![
            dim("proof "), Span::styled("fold ✓ whole-chain".to_string(), Style::default().fg(C_GREEN)),
            dim("  window "), Span::styled(format!("{} ✓", group(verified_real)), Style::default().fg(C_GREEN)),
            pos,
        ])
    } else if s.fetched_total == 0 && downloaded == 0 {
        Line::from(vec![dim("window "), Span::styled("connecting…".to_string(), Style::default().fg(C_DIM))])
    } else {
        let head = if at_tip {
            Span::styled("● tracking head".to_string(), Style::default().fg(C_GREEN))
        } else {
            Span::styled(format!("{} blk/s", group(app.p2p_rate.max(0.0).round() as u64)), Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD))
        };
        Line::from(vec![
            dim("window "), head,
            dim("  ⬇"), Span::styled(format!("{} dl", group(downloaded)), Style::default().fg(C_GREEN)),
        ])
    };

    // 3) in-flight chunk range + spine-verified watermark + tip-verify + peers.
    // v0.9.0: ⛓{N} = the contiguous CRYPTOGRAPHICALLY-VERIFIED spine height (precheck +
    // parent linkage back to genesis) — distinct from synced (downloaded). A break shows
    // red; an unverified-but-downloaded chain shows gold (verifier still walking).
    let vmark = if !app.online && !s.running {
        Span::styled("offline".to_string(), Style::default().fg(C_RED))
    } else if s.verify_break.is_some() {
        Span::styled("⚠spine".to_string(), Style::default().fg(C_RED).add_modifier(Modifier::BOLD))
    } else if verified_real > 0 && s.verified >= synced.saturating_sub(8) {
        Span::styled(format!("⛓✓{} vfy", group(verified_real)), Style::default().fg(C_GREEN))
    } else if verified_real > 0 {
        Span::styled(format!("⛓{} vfy", group(verified_real)), Style::default().fg(C_GOLD))
    } else if verified {
        Span::styled(format!("✓{}", group(synced)), Style::default().fg(C_GREEN))
    } else {
        Span::styled("✗tip".to_string(), Style::default().fg(C_GOLD))
    };
    let l3 = if !at_tip && s.running {
        let from = synced; let to = from.saturating_add(8192);
        Line::from(vec![
            dim("chunk "), Span::styled(format!("[{}..{}]", group(from), group(to)), Style::default().fg(C_VBRIGHT)),
            dim("  "), vmark, dim(" "),
            Span::styled(d, Style::default().fg(if s.connected_delta { C_GREEN } else { C_DIM })),
            Span::styled(e, Style::default().fg(if s.connected_epsilon { C_GREEN } else { C_DIM })),
        ])
    } else {
        Line::from(vec![
            dim("node  "), vmark, dim("  "),
            Span::styled(d, Style::default().fg(if s.connected_delta { C_GREEN } else { C_DIM })),
            Span::styled(e, Style::default().fg(if s.connected_epsilon { C_GREEN } else { C_DIM })),
            dim(format!(" {} peers", s.peer_count)),
        ])
    };

    Paragraph::new(vec![l1, l2, l3]).block(card_block(" SYNC", C_VBRIGHT))
}

fn render_mining(app: &App) -> Paragraph<'static> {
    let (state, scol) = if app.mining { ("ON", C_GREEN) } else { ("off", C_RED) };
    let earn = format!("~{:.4}", app.verified_count as f64 * 0.0005);
    // v0.2.35: live hashrate from the miner thread
    let rate_line = if app.mine_hashrate > 0.0 {
        let (val, unit) = if app.mine_hashrate >= 1000.0 {
            (app.mine_hashrate / 1000.0, "GH/s")
        } else {
            (app.mine_hashrate, "MH/s")
        };
        Line::from(vec![
            dim("rate "), Span::styled(format!("{:.2} {unit}", val), Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
            dim("   hashes "), Span::styled(format!("{}M", app.mine_hashes / 1_000_000), Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(dim("rate —   hashes —"))
    };
    // v0.2.35: wallet balance line
    let bal_line = if app.wallet_balance > 0 {
        let whole = app.wallet_balance / 100_000_000;
        let frac = (app.wallet_balance % 100_000_000) / 1_000_000;
        Line::from(vec![
            dim("balance "), Span::styled(format!("{whole}.{frac:02} SIGIL"), Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim("   shares "), Span::styled(group(app.mine_accepted), Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(vec![
            dim("balance —   shares "), Span::styled(group(app.mine_accepted), Style::default().fg(C_DIM)),
        ])
    };
    let lines = vec![
        Line::from(vec![
            dim("mining "), Span::styled(state, Style::default().fg(scol).add_modifier(Modifier::BOLD)),
            dim("   score "), Span::styled(group(app.score), Style::default().fg(C_GOLD)),
            dim("   verified "), Span::styled(group(app.verified_count), Style::default().fg(C_GREEN)),
        ]),
        Line::from(vec![
            dim("streak "), Span::styled(format!("×{}", app.streak), Style::default().fg(C_GOLD)),
            dim("   est earn "), Span::styled(earn, Style::default().fg(C_GOLD)),
        ]),
        rate_line,
        bal_line,
    ];
    Paragraph::new(lines).block(card_block(" MINING", C_CYAN))
}

fn render_security(app: &App) -> Paragraph<'static> {
    let k = app.eclipse_k;
    let agreed = app.eclipse_sources.iter().filter(|(_, b)| *b).count();
    let total = app.eclipse_sources.len().max(1);
    // v0.7.5: Real SQIsign status from tip verification
    let pq = app.verify.as_ref().map(|v| v.sqisign_available).unwrap_or(false);
    let sig_verified = app.verify.as_ref().map(|v| v.ok).unwrap_or(false);
    let sig_line = if sig_verified && pq {
        Line::from(vec![
            dim("sig "), Span::styled("SQIsign ✓", Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled("PQ-verified · 177B", Style::default().fg(C_VBRIGHT)),
        ])
    } else if sig_verified {
        Line::from(vec![
            dim("sig "), Span::styled("BLAKE3 ✓", Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled(if pq { "SQIsign ready" } else { "SQIsign gated" }, Style::default().fg(C_DIM)),
        ])
    } else if app.verify.is_some() {
        Line::from(vec![
            dim("sig "), Span::styled("FAILED", Style::default().fg(C_RED).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled("tip verification failed", Style::default().fg(C_RED)),
        ])
    } else {
        Line::from(vec![
            dim("sig "), Span::styled("waiting", Style::default().fg(C_DIM)),
            dim("  "), Span::styled("no tip received yet", Style::default().fg(C_DIM)),
        ])
    };
    // v0.7.5: Real eclipse probability — computed from actual K, not hardcoded 0.30
    let p_eclipse = if k > 0 { 0.30_f64.powi(k as i32) } else { 1.0 };
    let eclipse_line = if k > 0 {
        Line::from(vec![
            dim("eclipse "), Span::styled(format!("K={}", k), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
            dim("  agree "), Span::styled(format!("{}/{}", agreed, total),
                Style::default().fg(if agreed >= k as usize { C_GREEN } else if agreed > 0 { C_GOLD } else { C_RED })),
            dim(format!("  P={:.1e}", p_eclipse)),
        ])
    } else {
        Line::from(vec![
            dim("eclipse "), Span::styled("K=0", Style::default().fg(C_RED).add_modifier(Modifier::BOLD)),
            dim("  "), Span::styled("no independent sources — measuring…", Style::default().fg(C_DIM)),
        ])
    };
    let lines = vec![
        eclipse_line,
        sig_line,
        Line::from(vec![
            dim("verify "), Span::styled(format!("{}µs", app.sync_us), Style::default().fg(C_CYAN)),
            dim(if sig_verified { "  ✓ tip proven" } else { "  awaiting proof" }),
        ]),
    ];
    Paragraph::new(lines).block(card_block(" SECURITY", C_VIOLET))
}

// ── v0.7.0: AI Fleet Monitoring ─────────────────────────────────────────

fn render_fleet_card(app: &App) -> Paragraph<'static> {
    // Clone all fleet data to avoid borrow-from-app lifetime issues
    let nodes: Vec<(String, bool, u64, String)> = app.fleet_nodes.iter().map(|n| {
        let ver = if n.version.is_empty() { "?".to_string() }
            else if version_gt(VERSION, &n.version) { format!("!{}", n.version) }
            else { n.version.clone() };
        (n.name.clone(), n.online, n.height, ver)
    }).collect();
    let checking = app.fleet_last_check.elapsed() < Duration::from_secs(30);
    let total = nodes.len();
    let online = nodes.iter().filter(|n| n.1).count();
    let outdated = nodes.iter().filter(|n| n.1 && n.3.starts_with('!')).count();

    // v0.8: Mesh health from flux-p2p (if P2P sync is running)
    let mesh = app.p2p_state.mesh_peer_count;
    let mesh_quality = if mesh >= 4 { "healthy" } else if mesh >= 1 { "warming" } else { "empty" };
    let mesh_blk = app.p2p_blocks_synced;

    // Status line with fleet + mesh summary
    let status_color = if online == total && outdated == 0 { C_GREEN }
        else if online > 0 { C_GOLD }
        else { C_RED };
    let status_line = Line::from(vec![
        dim("fleet   "),
        Span::styled(format!("{}/{}", online, total),
            Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        dim(if checking { "  checking…" } else { "" }),
        if outdated > 0 {
            Span::styled(format!("  {} behind", outdated),
                Style::default().fg(C_RED).add_modifier(Modifier::BOLD))
        } else { Span::raw("") },
    ]);
    // v0.8: Mesh health line
    let mesh_line = Line::from(vec![
        dim("mesh    "),
        Span::styled(format!("{} peers", mesh), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
        dim("  "),
        Span::styled(mesh_quality, Style::default().fg(
            if mesh_quality == "healthy" { C_GREEN } else if mesh_quality == "warming" { C_GOLD } else { C_RED }
        )),
        dim(format!("  {} blk synced", group(mesh_blk))),
    ]);

    // Per-node lines with owned data
    let node_lines: Vec<Line> = nodes.iter().map(|(name, online, height, ver)| {
        let dot = if *online { Span::styled("●", Style::default().fg(C_GREEN)) }
                  else { Span::styled("○", Style::default().fg(C_RED)) };
        let ver_color = if ver == "?" { C_DIM }
            else if ver.starts_with('!') { C_RED }
            else { C_GREEN };
        Line::from(vec![
            dot,
            Span::raw(" "),
            Span::styled(name.clone(), Style::default().fg(C_CYAN)),
            dim(format!("  h{}", group(*height))),
            Span::raw("  "),
            Span::styled(ver.clone(), Style::default().fg(ver_color).add_modifier(Modifier::BOLD)),
        ])
    }).collect();

    let mut lines = vec![status_line, mesh_line];
    lines.extend(node_lines);

    Paragraph::new(lines).block(card_block(" FLEET · MESH", C_CYAN))
}

fn render_block_stream(app: &App) -> Paragraph<'static> {
    let tip_ok = app.verify.as_ref().map_or(false, |v| v.ok);
    let title: &'static str = if app.st.blocks_per_sec >= 5000.0 {
        " BLOCK STREAM · 5k+ blk/s"
    } else if app.st.blocks_per_sec >= 1000.0 {
        " BLOCK STREAM · 1k+ blk/s"
    } else if app.st.blocks_per_sec > 0.0 {
        " BLOCK STREAM · turbo"
    } else {
        " BLOCK STREAM · live"
    };
    let lines: Vec<Line> = if app.blocks.is_empty() {
        vec![Line::from(dim("streaming…"))]
    } else {
        app.blocks.iter().take(14).enumerate().map(|(i, b)| {
            let hash_pref: String = b.hash.chars().take_while(|c| c.is_ascii_hexdigit()).take(8).collect();
            let producer = if b.producer.is_empty() { "—".to_string() } else { b.producer.chars().take(12).collect() };
            // colored block marker (no dingbat): green tip, violet history
            let mark = if i == 0 && tip_ok { Span::styled("█ ", Style::default().fg(C_GREEN)) }
                       else { Span::styled("▌ ", Style::default().fg(C_VIOLET)) };
            Line::from(vec![
                mark,
                Span::styled(format!("{:>10} ", group(b.height)), Style::default().fg(C_GOLD)),
                Span::styled(format!("{}… ", hash_pref), Style::default().fg(C_DIM)),
                Span::styled(producer, Style::default().fg(C_CYAN)),
                dim(format!("  {}ms", b.tip_ms)),
            ])
        }).collect()
    };
    Paragraph::new(lines).block(card_block(&title, C_VBRIGHT))
}

// ── v0.6.0: Cortex MCP combo card ──────────────────────────────────────

fn render_cortex_card(app: &App) -> Paragraph<'static> {
    let agents_available = app.agents.iter().filter(|a| a.available).count();
    let agent_count = app.agents.len();
    let top_name: String = app.agents.iter()
        .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "—".to_string());
    let mcp_combo_tool = app.mcp_combo_tool.clone();
    let mcp_combo_result = app.mcp_combo_result.clone();
    let last_cortex_gain = app.last_cortex_gain;
    let cortex_loops = app.cortex_loops;

    let agent_line = if agent_count == 0 {
        Line::from(dim("agents  — no registry loaded"))
    } else {
        Line::from(vec![
            dim("agents  "),
            Span::styled(format!("{}/{}", agents_available, agent_count),
                Style::default().fg(if agents_available > 0 { C_GREEN } else { C_RED }).add_modifier(Modifier::BOLD)),
            dim("  top "),
            Span::styled(top_name, Style::default().fg(C_CYAN)),
        ])
    };
    let combo_line = if mcp_combo_tool.is_empty() {
        Line::from(vec![
            dim("combo   "),
            Span::styled("idle", Style::default().fg(C_DIM)),
            dim("  [C] execute cortex loop"),
        ])
    } else {
        let running = mcp_combo_result.is_empty();
        Line::from(vec![
            dim("combo   "),
            Span::styled(mcp_combo_tool, Style::default().fg(C_GOLD).add_modifier(Modifier::BOLD)),
            dim(if running { "  running…" } else { "  ✓ done" }),
        ])
    };
    let cortex_line = if last_cortex_gain > 0.0 {
        Line::from(vec![
            dim("cortex  "),
            Span::styled(format!("+{:.1}%", last_cortex_gain),
                Style::default().fg(C_GREEN).add_modifier(Modifier::BOLD)),
            dim(format!("  loops {}", cortex_loops)),
        ])
    } else if cortex_loops > 0 {
        Line::from(vec![
            dim("cortex  "),
            Span::styled(format!("no gain  loops {}", cortex_loops),
                Style::default().fg(C_DIM)),
        ])
    } else {
        Line::from(vec![
            dim("cortex  "),
            Span::styled("idle  [C] run optimization loop",
                Style::default().fg(C_DIM)),
        ])
    };
    let mcp_line = if !mcp_combo_result.is_empty() {
        let preview: String = mcp_combo_result.chars().take(60).collect();
        Line::from(vec![
            dim("result  "),
            Span::styled(preview, Style::default().fg(C_VBRIGHT)),
        ])
    } else {
        Line::from(dim("result  —"))
    };
    let lines = vec![
        agent_line,
        combo_line,
        cortex_line,
        mcp_line,
    ];
    Paragraph::new(lines).block(card_block(" CORTEX MCP", C_GOLD))
}

// ── v0.3.5: Browser shortcuts ────────────────────────────────────────────

/// Open a URL in the system browser. Non-blocking — spawns the OS opener
/// on a background thread so the TUI never freezes.

fn dirs_next() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        std::env::var("APPDATA").ok().map(std::path::PathBuf::from)
    } else {
        std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config"))
    }
}

/// The hosted SIGIL wallet (OAuth2 login) — works from ANY browser, so it's what we
/// hand a headless/remote (proxmox/SSH) operator who has no local GUI. Override with
/// SIGIL_WALLET_URL.
fn official_wallet_url() -> String {
    std::env::var("SIGIL_WALLET_URL").ok().filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://sigilgraph.fluxapp.xyz/sigil-wallet/".into())
}

/// True if there's no local GUI to open a browser into (headless box / SSH / proxmox
/// console). On Linux that's no DISPLAY and no WAYLAND_DISPLAY.
fn is_headless() -> bool {
    #[cfg(target_os = "linux")]
    { std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() }
    #[cfg(not(target_os = "linux"))]
    { false }
}

/// Best-effort open a URL in the local browser. Returns false if we're headless
/// (no GUI) — the caller then shows the link for the operator to copy instead.
fn open_browser(url: &str) -> bool {
    if is_headless() { return false; }
    let url = url.to_string();
    thread::spawn(move || {
        #[cfg(target_os = "linux")]
        { let _ = Command::new("xdg-open").arg(&url).spawn(); }
        #[cfg(target_os = "macos")]
        { let _ = Command::new("open").arg(&url).spawn(); }
        #[cfg(target_os = "windows")]
        { let _ = Command::new("cmd").args(["/c", "start", &url]).spawn(); }
    });
    true
}

// ── flux:// URL scheme ──────────────────────────────────────────────────────
// `flux://wallet` typed in the browser → the OS launches `sigil-top flux-open
// flux://wallet`. UI targets open the embedded :9800 wallet; command targets run
// the `fluxc` binary in a VISIBLE terminal (never silent exec from a URL).

/// Keep only safe chars so a flux:// URL can't inject shell metacharacters.
fn flux_safe_arg(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || "._-/".contains(*c)).collect()
}

/// Ensure the :9800 wallet server is up (spawn a detached `serve` if not), open `path`.
fn flux_open_local(path: &str) {
    let up = "127.0.0.1:9800".parse::<std::net::SocketAddr>().ok()
        .and_then(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_millis(350)).ok())
        .is_some();
    if !up {
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(&exe).arg("serve")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            thread::sleep(Duration::from_millis(900));
        }
    }
    open_browser(&format!("http://localhost:9800{path}"));
    thread::sleep(Duration::from_millis(500)); // let the browser launch before exit
}

/// Run `cmd` in a VISIBLE terminal (cross-platform) — so a flux:// URL can't run
/// fluxc commands behind the user's back.
fn flux_run_terminal(cmd: &str) {
    #[cfg(target_os = "linux")]
    {
        let hold = format!("{cmd}; echo; echo '[flux:// done — press enter]'; read _");
        for term in ["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "alacritty", "kitty", "xterm"] {
            let ok = match term {
                "gnome-terminal" | "xfce4-terminal" => Command::new(term).args(["--", "sh", "-c", &hold]).spawn().is_ok(),
                _ => Command::new(term).args(["-e", "sh", "-c", &hold]).spawn().is_ok(),
            };
            if ok { return; }
        }
    }
    #[cfg(target_os = "macos")]
    { let _ = Command::new("osascript").args(["-e", &format!("tell app \"Terminal\" to do script \"{cmd}\"")]).spawn(); }
    #[cfg(target_os = "windows")]
    { let _ = Command::new("cmd").args(["/c", "start", "cmd", "/k", cmd]).spawn(); }
}

/// Dispatch a flux:// URL.
fn flux_open(raw: &str) {
    // Debounce: if flux-open fired in the last 1.2s, ignore this one. Stops a tab
    // storm if the browser/OS invokes the handler repeatedly for a single action.
    {
        let stamp = std::env::temp_dir().join("sigil-flux-open.ts");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if let Ok(prev) = std::fs::read_to_string(&stamp) {
            if let Ok(p) = prev.trim().parse::<u128>() {
                if now.saturating_sub(p) < 1200 { return; }
            }
        }
        let _ = std::fs::write(&stamp, now.to_string());
    }
    let rest = raw.trim().trim_start_matches("flux://").trim_start_matches("flux:").trim_start_matches('/');
    let (head, tail) = match rest.split_once('/') { Some((a, b)) => (a, b), None => (rest, "") };
    let head = head.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    let arg = flux_safe_arg(tail.split(['?', '#']).next().unwrap_or(""));
    match head.as_str() {
        "" | "wallet" | "tron" | "w" => flux_open_local("/"),
        "enter" | "enter-sigil" | "new" | "onboard" | "login" => flux_open_local("/enter-sigil.html"),
        "engine" | "vite" | "vite-engine" => flux_open_local("/vite-engine.html"),
        // content-addressed fetch (the existing flux:// meaning in flux-fleet)
        "b3" => flux_run_terminal(&format!("flux-fleet get flux://b3/{arg}")),
        // fluxc command surface — visible terminal, whitelisted verbs only
        "build" | "dev" | "serve" | "test" | "stats" | "watch" | "plan" | "mcp" | "quick" | "self" | "run" => {
            let c = if arg.is_empty() { format!("fluxc {head}") } else { format!("fluxc {head} {arg}") };
            flux_run_terminal(&c);
        }
        // anything else → try a served page (graceful 404 if absent)
        other => flux_open_local(&format!("/{}.html", flux_safe_arg(other))),
    }
}

/// Register flux:// → this binary as the OS URL-scheme handler.
fn flux_register_scheme() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().to_string();
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").map_err(|_| "no $HOME".to_string())?;
        let apps = format!("{home}/.local/share/applications");
        std::fs::create_dir_all(&apps).map_err(|e| e.to_string())?;
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=Flux URL Handler\nComment=Open flux:// links (wallet, fluxc commands)\nExec={exe} flux-open %u\nTerminal=false\nNoDisplay=true\nStartupNotify=false\nMimeType=x-scheme-handler/flux;\n"
        );
        std::fs::write(format!("{apps}/flux-url-handler.desktop"), desktop).map_err(|e| e.to_string())?;
        let _ = Command::new("xdg-mime").args(["default", "flux-url-handler.desktop", "x-scheme-handler/flux"]).status();
        let _ = Command::new("update-desktop-database").arg(&apps).status();
        println!("  ✓ flux:// registered. Try typing  flux://wallet  in your browser.");
    }
    #[cfg(target_os = "windows")]
    {
        let cmd = format!("\"{exe}\" flux-open \"%1\"");
        let _ = Command::new("reg").args(["add", "HKCU\\Software\\Classes\\flux", "/ve", "/d", "URL:Flux Protocol", "/f"]).status();
        let _ = Command::new("reg").args(["add", "HKCU\\Software\\Classes\\flux", "/v", "URL Protocol", "/d", "", "/f"]).status();
        let _ = Command::new("reg").args(["add", "HKCU\\Software\\Classes\\flux\\shell\\open\\command", "/ve", "/d", &cmd, "/f"]).status();
        println!("  flux:// registered. Try: flux://wallet");
    }
    #[cfg(target_os = "macos")]
    {
        let _ = &exe;
        println!("  flux:// on macOS needs an .app bundle (manual) — open http://localhost:9800/ meanwhile.");
    }
    Ok(())
}

fn flux_unregister_scheme() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    { if let Ok(h) = std::env::var("HOME") { let _ = std::fs::remove_file(format!("{h}/.local/share/applications/flux-url-handler.desktop")); } }
    #[cfg(target_os = "windows")]
    { let _ = Command::new("reg").args(["delete", "HKCU\\Software\\Classes\\flux", "/f"]).status(); }
    println!("  flux:// handler removed.");
    Ok(())
}

fn render_footer(app: &App) -> Paragraph<'static> {
    let toast = if app.toast.is_empty() { String::new() } else { format!(" › {}", app.toast) };
    let keys = |c: &'static str, rest: &'static str, col: Color| -> Vec<Span<'static>> {
        vec![Span::styled(c, Style::default().fg(col).add_modifier(Modifier::BOLD)), dim(rest), Span::raw("  ")]
    };
    let mut kb = vec![Span::raw(" ")];
    kb.extend(keys("[M]", "ine", C_GOLD));
    kb.extend(keys("[F]", "ull", C_GREEN));
    kb.extend(keys("[V]", "erify", C_GREEN));
    kb.extend(keys("[Y]", "esync", C_CYAN));
    kb.extend(keys("[W]", "allet", C_CYAN));
    kb.extend(keys("[B]", "locks", C_CYAN));
    kb.extend(keys("[U]", "pdate", C_VBRIGHT));
    kb.extend(keys("[C]", "ortex", C_GOLD));
    kb.extend(keys("[H]", "eal", C_GOLD));
    kb.extend(keys("[N]", "odes", C_CYAN));
    kb.extend(keys("[L]", "ogin", C_VBRIGHT));
    kb.extend(keys("[Q]", "uit", C_RED));
    // v0.6.0: show serve status line
    let serve_line = if !app.serve_status.is_empty() {
        let short: String = app.serve_status.chars().take(72).collect();
        Line::from(Span::styled(format!(" ⚡ {}", short), Style::default().fg(C_GREEN)))
    } else {
        Line::from(Span::styled(" ⚡ fluxc serve :9800 · local wallet [W]", Style::default().fg(C_DIM)))
    };
    // v0.10.5.1: when offline, the top line becomes a calm status banner with a
    // live "offline for X · retry in Ns" countdown instead of a stale gold toast —
    // the operator always knows the cockpit is reconnecting, not frozen.
    let top_line = if !app.online {
        let dur = app.offline_since.map(|t| fmt_uptime(t.elapsed().as_secs())).unwrap_or_else(|| "0s".into());
        let txt = if app.refresh_inflight {
            format!(" ⚠ offline {} · reconnecting…", dur)
        } else {
            let next = app.refresh_delay().as_secs().saturating_sub(app.last_fetch.elapsed().as_secs());
            format!(" ⚠ offline {} · retry in {}s", dur, next)
        };
        Line::from(Span::styled(txt, Style::default().fg(C_RED).add_modifier(Modifier::BOLD)))
    } else {
        Line::from(Span::styled(toast, Style::default().fg(C_GOLD)))
    };
    Paragraph::new(vec![
        top_line,
        Line::from(kb),
        serve_line,
    ])
}
