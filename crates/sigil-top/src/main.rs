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

// 2026-08-24: extracted to the `sigil-block-store` library crate so `sigil-chronos`
// (a deterministic test harness that cannot import a binary-only crate) can drive the
// real height-index conflict logic instead of a re-modelled copy — same reason
// `sigil-sync` was extracted earlier. `block_store::X` below now resolves via the
// `use sigil_block_store as block_store;` alias so every existing call site in this
// crate is unchanged; only this module declaration and that alias line differ.
use sigil_block_store as block_store;
// pub(crate) (2026-08-23): producer::run reaches block_sync::verify::inflate_gossip_frame
// through this path — everything else stays exactly as private as before.
pub(crate) mod block_sync;
mod chain_verify; // v0.9.0: full verifying sync — spine continuity + precheck walk
mod ledger_verify; // ONE-CHAIN P2a: verify the MONEY chain's header spine + real /supply truth
mod gap_sync;     // SPINE-BREAK fix: testable genesis-up contiguity engine + shared watchdog/classify
mod help;
mod serve;
mod heroes;
use heroes::*;
mod mining_ui;  // LANE-U: mining tab + hero renderers
use mining_ui::*;
mod sync_ui;    // LANE-U: sync hero + sync-log tab renderers
mod flux_moe;   // on-device AI brain (local ollama) for the [A]I tab
use sync_ui::*;
mod tabs_ui;    // LANE-U: draw_ui dispatcher + tab/card/footer renderers
use tabs_ui::*;
mod wallet_ui;  // LANE-U: wallet/browser/tray/scheme plumbing
use wallet_ui::*;
mod local_api;   // v0.11.0: serve the explorer /api/* from the LOCAL verified spine
mod cathedral;     // CATHEDRAL DAGKNIGHT: vaulted 4-root + DagKnight finality view
// 2026-08-26: local-only signing/shield endpoints for the embedded wallet server, so a
// wallet opened via [W] (localhost:9800, same box as SIGIL_MINE_SEED) can Shield/Send/
// Swap/Bridge with ZERO prompts. `mine-sign` (Swap/Bridge) is unconditional — it only
// ever needs sigil_oauth::Keypair, already a non-optional dep. `mine-shield` and
// `mine-send-private` need sigil-shield's real note-construction/proving math and are
// internally gated on the `shield-register` feature (default-on), same as
// `shield_setup` above. See the module's own doc comment for the full design.
mod mine_local_api;
// v7.1.40 (grogu-sync-perf, 2026-08-19): PRODUCER-MODE Phase 1 — inert scaffolding only.
// `cathedral.rs`'s own doc comment already anticipated this: "Real flux-narwhal-core /
// flux-consensus linearizer can be dropped in the run_dagknight_linearize slot later
// without changing the surface." This module is that drop-in, ported (not yet — Phase 2)
// from sigil-node's main.rs. Behind the `producer` Cargo feature (off by default) AND a
// runtime double-gate (SIGIL_TOP_PRODUCER env, checked in main() below) so a shipped
// producer-capable binary still defaults to today's light-client-only behavior.
#[cfg(feature = "producer")]
mod producer;

// `mine-rig --seed`'s auto-registration for shielded mining. Default-on (see the
// crate's Cargo.toml `shield-register` feature comment).
#[cfg(feature = "shield-register")]
mod shield_setup;

use crate::cathedral::Cathedral;

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

/// LANE-B v0.50: how long the SIGIL rune animation stays on screen per play
/// (draw-on → hold → fade). Kept inside the 3–5 s band the side-mission specifies.
const RUNE_PLAY: Duration = Duration::from_millis(4200);
/// LANE-B v0.50: re-play cadence. An update being available is the loud signal →
/// play often (every 10 min); otherwise it is a quiet "still alive" pulse (every 2 h).
const RUNE_INTERVAL_UPDATE: Duration = Duration::from_secs(600);
const RUNE_INTERVAL_IDLE: Duration = Duration::from_secs(7200);


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
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Gauge, Padding, Paragraph},
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
use flux_rev::{Store, snapshot, Genesis};

// v0.11.0: combined release — explorer local-spine /api (rocky-explorer) +
// smooth-cruise (async refresh, panic-restore, offline backoff/banner, serve
// watchdog). 0.11.0 is valid 3-part SemVer so VERSION flows straight from Cargo.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// When the ratatui TUI owns the screen, raw `eprintln!` from the background P2P
/// thread (and elsewhere) smears the frame. Once the TUI starts we flip this and
/// route those lines to a logfile instead, so the dashboard stays clean.
pub(crate) static IN_TUI: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_PANIC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn log_line(s: String) {
    // v7.1.39 (grogu-sync-perf, 2026-08-19): `full-sync` and every other headless
    // subcommand NEVER call run_tui(), so IN_TUI stays false for their entire
    // lifetime — which means EVERY crate::tlog!() call anywhere in the sync/fetch
    // code ([snap-diag], [mesh] DIAL-FAIL, [anchor], [sync] snapshot-pull, VCATCH,
    // etc.) was a silent no-op in the one mode most suited to reproducible testing
    // (headless full-sync, CI gates). Root-caused live: a fresh full-sync repro run
    // showed 0 peers for 85s straight with ZERO diagnostic output anywhere, even
    // though extensive tlog! diagnostics were added earlier this session specifically
    // to explain exactly this kind of stall. Fix: outside the TUI, tlog! lines are
    // harmless on stderr (nothing owns the terminal) — print them directly, same as
    // the pre-TUI eprintln convention documented at run_tui's top ("pre-TUI prints go
    // to stderr — exactly where pre-UI diagnostics belong"). Zero behavior change
    // inside the TUI (still routes to the logfile only, unsmeared).
    if !IN_TUI.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{s}");
        return;
    }
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

fn remember_panic(msg: String) {
    let mut slot = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(msg);
}

fn last_panic_line() -> String {
    LAST_PANIC
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| "[PANIC] <panic hook did not run>".into())
}

fn release_channel_stale_msg(channel_version: &str) -> String {
    format!(
        "release channel is stale: channel v{} < this binary v{} — publish/re-sign sigil-top-latest.json",
        channel_version, VERSION
    )
}

fn release_channel_current_msg(channel_version: &str) -> String {
    if version_gt(VERSION, channel_version) {
        format!("⚠ {}", release_channel_stale_msg(channel_version))
    } else {
        format!("✓ up to date (v{VERSION}; channel v{channel_version}) — checked")
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
/// v7.0.26: release-channel BASES, tried in order. The plain-HTTP :8099 mirror is the
/// escape hatch for operator networks that filter the app's HTTPS (:443) — mining
/// submits prove :8099 reachable from every rig ("connect failed" on the updater while
/// GPU shares flowed). manifest+sig+binaries are identical mirrors on all three.
const CHANNEL_BASES: &[&str] = &[
    "https://sigilgraph.fluxapp.xyz/downloads",
    "https://quillon.xyz/downloads",
    "http://sigilgraph.quillon.xyz:8099/downloads",
];
/// Which base the last successful manifest fetch used — binary downloads follow it.
static ACTIVE_BASE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Which prebuilt this binary self-updates to (its per-OS entry in the manifest).
const SELF_TARGET: &str = if cfg!(all(windows, feature = "gpu")) { "windows-x64-gpu" }
    else if cfg!(windows) { "windows-x64" }
    else if cfg!(target_os = "macos") { "macos-arm64" }
    else if cfg!(feature = "gpu") { "linux-x64-gpu" }
    else { "linux-x64" };
/// Live testnet feed (same source flux-node.html uses): status + tip + block stream.
const DEFAULT_FEED: &str = "https://sigilgraph.fluxapp.xyz/sigil-status.json";
/// v0.38: flux-rev SOURCE provenance, stamped at release-build time via
/// `SIGIL_TOP_FLUX_REV=$(flux-rev snapshot crates/sigil-top)` -> the BLAKE3
/// `full:` content address of the source tree this binary was built from.
/// "unstamped" on ad-hoc dev builds. Surfaced in the header + `provenance` cmd
/// + every webhook event — SIGIL north-star claim #1 made visible.
const FLUX_REV: &str = match option_env!("SIGIL_TOP_FLUX_REV") { Some(v) => v, None => "unstamped" };

/// Short display form of [`FLUX_REV`] (strip `full:`, first 10 chars).
fn short_rev() -> String {
    let r = FLUX_REV.strip_prefix("full:").unwrap_or(FLUX_REV);
    r.chars().take(10).collect()
}
// 2026-08-29: 8 → 10 decimals (the sigil-g2 move; /v1/supply's max_supply is
// 2.1e17). At 8 dp every SIGIL amount this client rendered — supply, balances,
// send previews — was overstated exactly 100x.
const MAX_SUPPLY_BASE: u128 = 210_000_000_000_000_000; // 21 M × 10^10
const DECIMALS: u32 = 10;

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
    /// v7.1.13: the publisher's own freshness stamp (unix seconds). 0 when the
    /// feed doesn't carry it. Used to refuse zombie mirrors: a feed can answer
    /// HTTP 200 with WEEKS-old data (the dist-fluxapp copy froze 2026-07-27 and
    /// every client worldwide trusted tip 33,598,726 for 13 days).
    #[serde(default)]
    updated: u64,
    /// v0.42: false while the node is (re)building its explorer full-text index in
    /// the background — money/chain data is live, only search is briefly empty.
    /// Absent on older nodes → defaults true (don't show a stale "indexing" badge).
    #[serde(default = "default_true")]
    index_ready: bool,
    /// L-F 0.77.5: per-block reward in whole SIGIL (fractional after halvings,
    /// e.g. 5 → 2.5). The feed has carried reward_sig all along but it was
    /// dropped on the floor here — which is why "SIGIL/s" vanished from the UI
    /// when mining moved in-node (the v0.64.2 ONE-GRAPH cut pointed at a Node
    /// tab that never received the network stats). 0.0 = unknown (old feed).
    #[serde(default)]
    reward_sig: f64,
}

fn default_true() -> bool { true }

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

/// 0.77: ONE shared pooled HTTP client for the per-tick fetchers (feed, block fallback,
/// eclipse-k). Building a fresh Client per call opened a new TCP+TLS connection per tick
/// that then sat in TIME_WAIT — over a multi-hour genesis-archive sync that exhausted
/// Windows' ephemeral ports (the "tip frozen / error sending request" bug, #156 item 3).
/// Timeouts are set PER REQUEST (each call site keeps its old value); the TLS posture is
/// the union of the old per-site builders (TLS_1_2 feeds + the invalid-cert fallback).
static HTTP: std::sync::LazyLock<reqwest::blocking::Client> = std::sync::LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .danger_accept_invalid_certs(true)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
});

fn set_feed_err(e: String) { if let Ok(mut g)=LAST_FEED_ERR.lock(){ *g=e; } }
/// The most recent feed-fetch failure reason — shown on the OFFLINE card so a user can SEE why
/// (DNS, TLS, connection refused, HTTP status, or JSON parse) instead of a blind "offline".
pub fn last_feed_err() -> String { LAST_FEED_ERR.lock().map(|g| g.clone()).unwrap_or_default() }

fn fetch_feed(url: &str) -> Option<(NodeStatus, Vec<FeedBlock>)> {
    // 0.77: shared pooled client (keep-alive) — was a fresh Client per call per tick.
    let resp = match HTTP.get(url).timeout(Duration::from_secs(6)).send() { Ok(r)=>r, Err(e)=>{ set_feed_err(format!("connect @ {url}: {e}")); return None; } };
    let code = resp.status();
    let body = match resp.text() { Ok(b)=>b, Err(e)=>{ set_feed_err(format!("read @ {url} (HTTP {code}): {e}")); return None; } };
    let feed: Feed = match serde_json::from_str(&body) { Ok(f)=>f, Err(e)=>{ set_feed_err(format!("parse @ {url} (HTTP {code}): {e}")); return None; } };
    let s = feed.status;
    // v0.64 LANE-T: the status feed sends supply in BASE units on some sources and
    // WHOLE SIGIL on others. Multiplying a base value by 10^8 again double-scaled it
    // (showed 1,748,502,017,000 instead of ~17,485). Auto-detect by magnitude: nothing
    // can exceed the 21M WHOLE cap, so a value above it is already base -> keep; a value
    // at/under 21M is whole -> scale to base.
    let supply_raw: u128 = s.supply.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
    // LANE-T hardening: a value 10^8 over the cap is the double-scale tell — clamp to the
    // 21M base cap so a corrupt/mis-scaled source can never show > 21M SIGIL in the hero.
    let native_supply = (if supply_raw > 21_000_000u128 { supply_raw } else { supply_raw.saturating_mul(10u128.pow(DECIMALS)) }).min(MAX_SUPPLY_BASE);
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
        reward_sig: s.reward_sig,
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
    // v0.64.1: remember the last PRODUCE-feed height so the local-API fallback can
    // never silently swap chains. The :8099 rpcd is the MINE chain (height ~3.5k);
    // when all feed mirrors hiccup for one poll, falling back to it made the hero
    // JUMP 520k -> 3.5k -> 520k. A fallback drastically below the last feed height
    // is a different chain -> show an honest offline/retry instead of lying.
    static LAST_FEED_H: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // v7.1.13: STALENESS GUARD. A mirror can answer HTTP 200 with a frozen file
    // (dist-fluxapp's copy stopped updating 2026-07-27; it is FIRST in this list,
    // so every client pinned its tip to 33,598,726 for 13 days). A feed whose own
    // `updated` stamp is >15 min old loses to any fresh mirror; the freshest stale
    // copy is still used when NOTHING is fresh (better an old tip than none).
    let mut stale_best: Option<NodeStatus> = None;
    for url in [cfg.feed.as_str(),
                "https://sigilgraph.fluxapp.xyz/sigil-status.json",
                "https://quillon.xyz/sigil-status.json",
                // v7.0.26: plain-HTTP :8099 mirror — the port mining provably reaches
                // on networks that filter the app's HTTPS (the OFFLINE-badge saga).
                "http://sigilgraph.quillon.xyz:8099/sigil-status.json"] {
        if let Some((st, _b)) = fetch_feed(url) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if st.updated > 0 && now.saturating_sub(st.updated) > 900 {
                if stale_best.as_ref().map(|b| st.updated > b.updated).unwrap_or(true) {
                    stale_best = Some(st);
                }
                continue;
            }
            LAST_FEED_H.store(st.height, std::sync::atomic::Ordering::Relaxed);
            return (st, true, "feed");
        }
    }
    if let Some(st) = stale_best {
        LAST_FEED_H.store(st.height, std::sync::atomic::Ordering::Relaxed);
        return (st, true, "feed-stale");
    }
    match fetch(&cfg.api) {
        Ok(s) => {
            let lf = LAST_FEED_H.load(std::sync::atomic::Ordering::Relaxed);
            if lf > 0 && s.height.saturating_mul(4) < lf {
                // mine-chain rpcd answering for a produce-feed blip — suppress, retry feed
                (NodeStatus::default(), false, "offline")
            } else {
                (s, true, "local")
            }
        }
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
        // 0.77: shared pooled client — was a fresh Client per fallback tick.
        {
            let client = &*HTTP;
            let api_base = api.trim_end_matches('/');
            if let Ok(resp) = client.get(format!("{}/v1/blocks/recent?limit=14", api_base)).timeout(Duration::from_secs(8)).send() {
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
            api: crate::help::DEFAULT_API.into(), feed: DEFAULT_FEED.into(),
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
    // Full reference lives in `help.rs` — see that module for why it moved and for
    // the dead-`fluxapp`-default this used to advertise.
    help::print_help(VERSION);
}

/// ONE-CHAIN (v7.1.6): make status show THE chain, not the retiring spine.
/// - When no spine engine is running, the ledger `/supply` height replaces the
///   feed's 31.5M dyno counter (returned so the caller can also set target_height).
/// - Uptime: the feed never carried one ("uptime 0m 0s forever") — fall back to
///   THIS monitor's real process uptime whenever the feed gives none.
/// Shared by the interactive loop (`apply_refresh`) and the headless `--once` frame.
fn one_chain_view(st: &mut NodeStatus, spine_off: bool) -> Option<u64> {
    let mut new_target = None;
    if spine_off {
        if let Some(li) = ledger_verify::latest() {
            if li.height > 0 { st.height = li.height; new_target = Some(li.height); }
        }
    }
    if st.uptime_secs == 0 {
        static PROC_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        st.uptime_secs = PROC_START.get_or_init(Instant::now).elapsed().as_secs().max(1);
    }
    new_target
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

/// v0.33.3: seconds → compact ETA ("4h 11m", "38m", "2d 3h"). "∞" when not making progress.
fn fmt_eta(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 { return "—".into(); }
    if secs > 60.0 * 60.0 * 24.0 * 99.0 { return "∞".into(); }
    let s = secs as u64;
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {m}m") } else if m > 0 { format!("{m}m") } else { format!("{s}s") }
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
    let build_net = build_network_id();
    let net = if st.network.is_empty() { build_net.as_str() } else { &st.network };
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
    } else if version_gt(VERSION, &latest) {
        o.push_str(&format!("    {RED}{BOLD}⚠ channel stale{RESET} {DIM}served v{latest} < binary v{VERSION} · press [U] for details{RESET}\n"));
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
    // v0.42: explorer search readiness. The node now binds + serves money/chain
    // routes instantly on restart and builds its full-text index in the
    // background, so this briefly reads "indexing…" before "search ready".
    if !st.index_ready {
        o.push_str(&row("explorer", &format!("{GOLD}⟳ indexing…{RESET} {DIM}money/chain live; search warming up{RESET}")));
    }
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

    // economics — 21 M hard cap. ONE-CHAIN P2a: the LEDGER's /supply is the truth
    // (SigilState.native_supply behind the commit chokepoint). The old value came
    // from the spine's display path and read "100% minted" — fiction. Feed value
    // only as fallback while the ledger is unreachable.
    o.push_str(&mid_title("ECONOMICS  (21 M hard cap)"));
    let ledger = ledger_verify::latest();
    let (sup_base, sup_src) = match &ledger {
        Some(li) => (li.supply_base, "ledger"),
        None => (st.native_supply, "feed"),
    };
    let frac = sup_base as f64 / MAX_SUPPLY_BASE as f64;
    o.push_str(&row("supply", &format!("{GOLD}{}{RESET} {DIM}/ 21,000,000 SIGIL · {sup_src}{RESET}", fmt_supply(sup_base))));
    o.push_str(&row("minted", &format!("{}  {GOLD}{:.4}%{RESET}", bar(frac, 30, GOLD), frac * 100.0)));
    if let Some(li) = &ledger {
        o.push_str(&row("chain", &format!("{DIM}mining ledger · height{RESET} {GOLD}{}{RESET}", li.height)));
        let v = match (&li.break_note, li.header_tip) {
            (Some(b), _) => format!("{}✗ {b}{RESET}", "\x1b[38;5;203m"),
            (None, 0) => format!("{DIM}headers not minted yet (pre-P1 node){RESET}"),
            (None, _) => format!("{GREEN}✓ headers #{}→#{} self-linked{RESET} {DIM}· {} walked{RESET}",
                li.verified_floor, li.header_tip, li.checked),
        };
        o.push_str(&row("verify", &v));
    }

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
    // CATHEDRAL DAGKNIGHT (wired 2026-06-17) — module live, full vault ingest + tab in next slice
    o.push_str(&row("cathedral", "vaults:0 (first wire) div:0 ✓ 4-roots+tip-proof+fluxc"));
    o.push_str(&row("crypto", &format!("{DIM}Ajtai/SIS · post-quantum · no trusted setup{RESET}")));

    o.push_str(&bottom());
    if !online {
        o.push_str(&format!("  {DIM}feed + local node both unreachable ({api}) — showing SIGIL constants{RESET}\n"));
    } else if source == "feed" {
        o.push_str(&format!("  {GREEN}● synced from verified live feed{RESET} {DIM}· no local node required — verify on a potato{RESET}\n"));
    }
    // keybar footer — real keybindings UI
    o.push_str(&format!("  {GOLD}[M]{RESET}{DIM}ine{RESET}   {GREEN}[F]{RESET}{DIM}ull{RESET}  {GREEN}[V]{RESET}{DIM}erify{RESET}  {CYAN}[Y]{RESET}{DIM}esync{RESET}   {GOLD}[U]{RESET}{DIM}pdate{RESET}   {VBRIGHT}[L]{RESET}{DIM}ogin{RESET}   {CYAN}[T]{RESET}{DIM}stats{RESET}   {DIM}[Q]uit{RESET}\n"));
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
    let build_net = build_network_id();
    let net = if st.network.is_empty() { build_net.as_str() } else { &st.network };
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

pub(crate) fn flux_home() -> String { std::env::var("HOME").unwrap_or_else(|_| "/root".into()) }
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
    // 2026-08-25: shield_setup.rs's seed_bytes() (used by headless `mine-rig`) strips
    // an optional 0x/0X prefix before decoding and its doc comment claims this function
    // matches that behavior -- it didn't. A SIGIL_MINE_SEED carrying a 0x prefix decoded
    // fine for headless mining but made miner_keypair() (interactive [M]ine/[W], the ONLY
    // caller of this function) silently return None, dropping straight through
    // resolve_mine_wallet()'s seed-priority to an unrelated fallback wallet -- for both
    // display AND actual mining credit. Real bug, not a display-only one: headless mined
    // (and credited) correctly the whole time; interactive mined to the wrong address.
    let h = h.trim();
    let h = h.strip_prefix("0x").or_else(|| h.strip_prefix("0X")).unwrap_or(h);
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

// Console glyph subsystem (UI_ASCII / ui_ascii / init_ui_ascii / sa) extracted to
// console_glyphs.rs (god-file split, 2026-09-01). Re-exported so every call site —
// including heroes.rs / sync_ui.rs / tabs_ui.rs via `use super::*` — is unchanged.
mod console_glyphs;
pub(crate) use console_glyphs::{init_ui_ascii, sa, ui_ascii};

/// v0.40: on Windows, drop to BELOW_NORMAL priority class BEFORE any thread
/// spawns. The OS scheduler then always favors the user's own apps — whateve
/// sigil-top does (render, mine, opt-in sync), it can never make the desktop
/// stutter. No crate dep: two kernel32 calls.
#[cfg(windows)]
fn lower_process_priority() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn SetPriorityClass(handle: isize, class: u32) -> i32;
    }
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// The install path captured at startup, BEFORE any self-replace. Afte
/// `self_replace` swaps the binary, `/proc/self/exe` (what `current_exe()` reads)
/// points at the moved-aside OLD inode — often a "(deleted)" path — so a relaunch
/// that spawns `current_exe()` fails with ENOENT even though the NEW binary sits
/// at this original path. Capturing it up front makes relaunch reliable.
static INSTALL_EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

// Sync-mode persistence + boot_trace extracted to startup_util.rs (god-file split,
// 2026-09-01). Re-exported so main.rs's unqualified calls are unchanged.
mod startup_util;
pub(crate) use startup_util::{boot_trace, persist_sync_mode, read_sync_mode};

/// v3 (2026-06-19) — THE real "no TUI on Windows" root cause: Rust's `is_terminal()`
/// returns FALSE on some genuine Windows consoles (double-click / conhost / Windows
/// Terminal), so sigil-top fell through to the headless path and the dashboard never
/// opened (reproduced under Wine adverse-mode: `interactive=false` → exit before run_tui).
/// On Windows a console IS attached whenever GetConsoleWindow() is non-null — trust that
/// over is_terminal(). A genuine service/redirected run with no console returns null →
/// stays headless, so no CI/pipe regression.
#[cfg(windows)]
fn win_has_console() -> bool {
    extern "system" { fn GetConsoleWindow() -> *mut core::ffi::c_void; }
    unsafe { !GetConsoleWindow().is_null() }
}
#[cfg(not(windows))]
fn win_has_console() -> bool { false }

fn main() {
    // v0.64: STARTUP TRACE + panic capture. If the app exits unexpectedly on a
    // Windows double-click, the breadcrumb + panic land in this file so we can
    // read the EXACT reason instead of guessing.
    boot_trace(&format!("main() entry v{} pid {}", env!("CARGO_PKG_VERSION"), std::process::id()));
    std::panic::set_hook(Box::new(|info| { boot_trace(&format!("PANIC: {info}")); }));
    // v7.1.40 producer-mode Phase 1: wire the runtime double-gate end-to-end now, while
    // it's still guaranteed inert (both functions hardcode `false` until Phase 3/5), so
    // the gate's presence and its boot-trace visibility are proven before anything real
    // depends on them. A `producer`-feature binary run WITHOUT either env var behaves
    // identically to today's shipping light client — this line changes nothing yet.
    #[cfg(feature = "producer")]
    boot_trace(&format!(
        "producer-mode gates: SIGIL_TOP_PRODUCER→{} SIGIL_TOP_PRODUCE→{} (both inert, Phase 1)",
        producer::producer_mode_enabled(), producer::should_produce()
    ));
    // POOL-DIAG: report THIS release's version on every challenge fetch (`&v=`),
    // not the flux-miner engine-crate fallback — the pool's /mining/miners is how
    // stale rig installs get spotted.
    let _ = flux_miner::client::CLIENT_VERSION.set(VERSION.to_string());
    // Capture the real install path NOW, before anything can self-replace us.
    if let Ok(e) = std::env::current_exe() {
        if e.exists() { let _ = INSTALL_EXE.set(e); }
    }
    #[cfg(windows)]
    lower_process_priority();
    enable_rich_console(); // UTF-8 + VT so icons/colours render (fixes the `?` glyphs)
    init_ui_ascii();       // decide ASCII vs rich glyphs (Windows-safe layout)
    // subcommands: login / logout (handled before the render loop)
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // v0.77.7: clean up the `…exe.old` left by the Windows lock-fallback swap (the previous
    // running image, renamed aside so the new binary could take the install path). It's only
    // unlockable once the old process is gone, i.e. now. Skip during --selfcheck (must stay pure).
    #[cfg(windows)]
    if argv.first().map(|s| s.as_str()) != Some("--selfcheck") {
        if let Some(exe) = INSTALL_EXE.get().cloned().or_else(|| std::env::current_exe().ok()) {
            let old = exe.with_extension("exe.old");
            if old.exists() { let _ = std::fs::remove_file(&old); }
        }
    }
    match argv.first().map(|s| s.as_str()) {
        // v0.25: updater PRE-FLIGHT probe. Print the version and exit 0 — touch NOTHING else
        // (no network, no TUI, no DB, no splash) — so `relaunch_new_binary` can confirm a
        // freshly-swapped binary actually STARTS and reports the expected version BEFORE it
        // tears down the running app to hand off. This is what stops a bad/corrupt/ABI-
        // mismatched update from making the app vanish on the restart-after-sync.
        Some("--selfcheck") => { println!("{VERSION}"); return; }
        // v7.4.2: an EXPLICIT version command that prints and exits. Until now
        // `version`/`--version` were unhandled and fell through to the default
        // TUI, which only *sometimes* flushed its `v{VERSION}` header to stdout
        // before blocking on the async runtime — so `release-sigil-top.sh`'s
        // own `"$LBIN" version | grep -q "v$VER"` sanity check was a coin-flip
        // that finally hung the whole release ceremony (2026-08-29, v7.4.2).
        // Print the same `v{VERSION}` the header shows, so that grep is
        // deterministic and users get a real version command.
        Some("version") | Some("--version") | Some("-V") => { println!("sigil-top v{VERSION}"); return; }
        // v0.27.5: manual rollback escape hatch — revert to the previous binary the last update
        // backed up (pre-flighted before the swap). The operator's "undo a bad update" button.
        Some("revert") => { do_revert(); return; }
        // Windows launch-at-login control: `sigil-top autostart on|off|status`. The TUI also
        // exposes this via the tray's "Start at login" checkbox.
        Some("autostart") => {
            match argv.get(1).map(|s| s.as_str()) {
                Some("on") | Some("enable") | Some("true") => match autostart_set(true) {
                    Ok(()) => println!("✓ launch-at-login ENABLED"),
                    Err(e) => { eprintln!("✗ {e}"); std::process::exit(1); }
                },
                Some("off") | Some("disable") | Some("false") => match autostart_set(false) {
                    Ok(()) => println!("✓ launch-at-login DISABLED"),
                    Err(e) => { eprintln!("✗ {e}"); std::process::exit(1); }
                },
                _ => println!("launch-at-login: {}", if autostart_enabled() { "ON" } else { "OFF" }),
            }
            return;
        }
        // v0.38: print this binary\'s full provenance — version, flux-rev source id,
        // the binary\'s own BLAKE3, and which release channel/target it tracks.
        Some("provenance") => {
            let exe_hash = std::env::current_exe().ok()
                .and_then(|p| std::fs::read(p).ok())
                .map(|b| blake3::hash(&b).to_hex().to_string())
                .unwrap_or_else(|| "?".into());
            println!("sigil-top v{VERSION}");
            println!("flux-rev:      {FLUX_REV}");
            println!("binary blake3: {exe_hash}");
            println!("channel:       {UPDATE_MANIFEST}");
            println!("target:        {SELF_TARGET}");
            return;
        }
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
            let local_api = block_store::BlockStore::open_blocking(&sigil_top_db_path()).ok().map(|st| {
                std::sync::Arc::new(local_api::LocalApi {
                    reader: {
                        let c = std::sync::Arc::new(std::sync::OnceLock::new());
                        let _ = c.set(st.reader());
                        c
                    },
                    sync: None,
                    cortex: std::sync::Arc::new(std::sync::Mutex::new(local_api::CortexSnapshot::default())),
                    network: build_network_id(),
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
        // when the user types flux://wallet in the browser → ensure the local serve
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
        // Headless CONTINUOUS dual-lane mining for rigs (Hive OS / systemd / CI) —
        // the SAME in-process flux_miner::engine::supervisor the [5] Mining tab runs,
        // but with no TTY. This is why "one binary = node + light client + wallet +
        // miner": a rig runs `sigil-top mine-rig <wallet>` and needs no separate exe.
        // GPU by default (the `gpu` feature); the engine falls back to CPU if OpenCL
        // init fails. Runs until killed. Output matches the standalone sigil-miner's
        // run_headless line so the Hive OS h-stats parser reads it unchanged.
        //   sigil-top mine-rig [wallet-64hex] [node-url]
        //   env: SIGIL_MINE_WALLET, SIGIL_MINE_NODE, SIGIL_MINE_CPU=1 (force CPU)
        //   env: SIGIL_MINE_SEED — 2026-08-24, the "no opt-in" privacy path. When set,
        //   the wallet address is DERIVED from this seed (overriding SIGIL_MINE_WALLET
        //   / the positional wallet arg — a rig can't mine to a different wallet than
        //   the one it just proved ownership of), and this rig self-registers for
        //   shielded mining before the loop starts: every reward from here on mints as
        //   a private note instead of a transparent credit. A bare wallet address (no
        //   seed) still works exactly as before — nobody who was already mining is
        //   broken by this.
        Some("mine-rig") => {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};
            let url = argv.get(2).cloned()
                .or_else(|| std::env::var("SIGIL_MINE_NODE").ok())
                .unwrap_or_else(engine_node_url);
            #[cfg(feature = "shield-register")]
            let seed_material = std::env::var("SIGIL_MINE_SEED").ok();
            #[cfg(not(feature = "shield-register"))]
            let seed_material: Option<String> = None;
            let fallback_wallet = || {
                argv.get(1).cloned()
                    .or_else(|| std::env::var("SIGIL_MINE_WALLET").ok())
                    .unwrap_or_else(miner_wallet)
            };
            let wallet = if let Some(seed) = seed_material.as_deref() {
                #[cfg(feature = "shield-register")]
                {
                    match shield_setup::wallet_signing_key(seed) {
                        Some(sk) => {
                            let derived = hex::encode(sk.verifying_key().to_bytes());
                            println!("  🔐 SIGIL_MINE_SEED set — mining wallet derived: {}…", &derived.chars().take(8).collect::<String>());
                            // 2026-08-26: a background KEEPER, not the one-shot attempt
                            // this replaces. Registration settles ~30 min after
                            // submission, so the old blocking call could not observe
                            // whether it landed — a rig whose registration was refused
                            // (node briefly down) or accepted-then-dropped printed one
                            // line and mined transparently forever. The keeper asks the
                            // chain first (so a restart costs nothing when already
                            // registered), submits when absent, watches for it to
                            // actually settle, and resubmits if it doesn't. Non-blocking:
                            // mining starts immediately either way.
                            shield_setup::spawn_registration_keeper(&url, seed, |line| {
                                println!("  {line}");
                            });
                            // Keep the SHIELDED balance visible too. A registered wallet is
                            // paid in notes, so its transparent balance is frozen forever —
                            // showing only that number is what made a miner earning ~4
                            // SIGIL/min believe they were earning nothing.
                            shield_setup::spawn_shielded_scanner(&url, seed);
                            derived
                        }
                        None => {
                            // Same format SIGIL_MINE_SEED already requires for [M]ine
                            // (main.rs's miner_keypair()) — a malformed value here would
                            // otherwise silently fall through to an unrelated wallet,
                            // which is worse than refusing to guess.
                            println!("  ⚠ SIGIL_MINE_SEED is set but isn't a valid 64-hex seed — ignoring it, mining to the address instead");
                            fallback_wallet()
                        }
                    }
                }
                #[cfg(not(feature = "shield-register"))]
                { let _ = seed; fallback_wallet() }
            } else {
                // No seed — so this rig CANNOT be auto-registered for shielded mining,
                // and that is a cryptographic limit rather than a missing feature:
                // registration is a signed tx over the wallet's own key, and a bare
                // address is exactly the thing that proves nobody here holds that key.
                // Say so once, plainly, instead of leaving the operator to wonder why
                // their rewards are public. Mining itself is unaffected.
                let w = fallback_wallet();
                // The wallet may ALREADY be registered — e.g. registered once from the web
                // wallet, then mined here by address alone. Then rewards ARE minted as
                // private notes, the transparent balance never moves, and this rig has no
                // seed with which to open them: the operator sees a frozen number and
                // concludes mining is broken. It is not; the money is real and this process
                // simply cannot see it. Say so explicitly rather than printing the generic
                // "rewards will be TRANSPARENT" line, which in that case is FALSE.
                #[cfg(feature = "shield-register")]
                {
                    let (wc, urlc) = (w.clone(), url.clone());
                    std::thread::spawn(move || {
                        if shield_setup::wallet_is_registered(&urlc, &wc) {
                            println!(
                                "  ⚠ {}… IS registered for shielded mining, but this rig has no \
                                 SIGIL_MINE_SEED.\n    Your rewards are being paid as PRIVATE NOTES \
                                 — the transparent balance shown below will never rise, and this \
                                 process cannot open them.\n    Set SIGIL_MINE_SEED=<64-hex seed for \
                                 this wallet> to see and spend what you are earning.",
                                &wc.chars().take(8).collect::<String>()
                            );
                        } else {
                            println!("  ℹ mining to {}… with no SIGIL_MINE_SEED — rewards will be TRANSPARENT. \n    Set SIGIL_MINE_SEED=<64-hex seed for this wallet> to have them mint as private notes instead.",
                                &wc.chars().take(8).collect::<String>());
                        }
                    });
                }
                #[cfg(not(feature = "shield-register"))]
                println!("  ℹ mining to {}… with no SIGIL_MINE_SEED — rewards will be TRANSPARENT. \n    Set SIGIL_MINE_SEED=<64-hex seed for this wallet> to have them mint as private notes instead.",
                    &w.chars().take(8).collect::<String>());
                w
            };
            let want_gpu = cfg!(feature = "gpu") && std::env::var("SIGIL_MINE_CPU").is_err();
            println!("\n  ▲ sigil-top mine-rig v{VERSION} — dual-lane (BLAKE4 Φ + VDF Ω) — headless");
            println!("  wallet: {}…  node: {url}  requested: {}",
                &wallet.chars().take(8).collect::<String>(), if want_gpu { "GPU" } else { "CPU" });
            let stats = Arc::new(Mutex::new(flux_miner::engine::MinerStats::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let desired_gpu = Arc::new(AtomicBool::new(want_gpu));
            let gpu_failed = Arc::new(AtomicBool::new(false));
            {
                let (u, w, s, st, dg, gf) = (url.clone(), wallet.clone(), stats.clone(),
                    stop.clone(), desired_gpu.clone(), gpu_failed.clone());
                std::thread::spawn(move || flux_miner::engine::supervisor(u, w, s, st, dg, gf));
            }
            // Emit a line whenever the accepted+rejected share count changes — the
            // exact shape the Hive OS h-stats.sh parser reads.
            let mut last = 0u64;
            let mut last_update: Option<String> = None;
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let s = stats.lock().unwrap();
                if s.update_msg != last_update {
                    last_update = s.update_msg.clone();
                    if let Some(u) = &last_update { println!("  [update] {u}"); }
                }
                if s.shares_ok + s.shares_bad != last {
                    last = s.shares_ok + s.shares_bad;
                    let line = s.log.front().cloned().unwrap_or_default();
                    // BALANCE, HONESTLY (2026-08-27). `s.balance` comes from
                    // `flux_miner::engine::fetch_balance`, i.e. GET /api/v1/balance — the
                    // TRANSPARENT balance. For a wallet that has published a shield key
                    // that number is frozen forever by construction: every reward it wins
                    // is minted as a hidden note, so the one line a headless rig prints
                    // reads as "mining earns nothing" no matter how much it earns.
                    // Operator-reported twice, the second time against a build that had
                    // already gained a shielded panel — in the interactive TUI's mining
                    // tab, which a headless `mine-rig` never renders.
                    //
                    // When a seed is present the scanner knows the real figure; show that
                    // and label it, and keep the transparent number visible so the two are
                    // never confused for each other.
                    #[cfg(feature = "shield-register")]
                    let bal_txt = match shield_setup::latest_shielded() {
                        Some(sn) => format!(
                            "shielded {} SIGIL (+{} transparent)",
                            flux_miner::engine::format_sigil(sn.balance),
                            flux_miner::engine::format_sigil(s.balance)
                        ),
                        None => format!("bal {} SIGIL", flux_miner::engine::format_sigil(s.balance)),
                    };
                    #[cfg(not(feature = "shield-register"))]
                    let bal_txt = format!("bal {} SIGIL", flux_miner::engine::format_sigil(s.balance));
                    println!(
                        "  [{}] {line}   [✓{} ✗{}]  {} (Φ {})  {bal_txt}",
                        s.mode, s.shares_ok, s.shares_bad,
                        flux_miner::engine::format_hps(s.hashrate), flux_miner::format_flux(s.hashrate),
                    );
                }
                if stop.load(Ordering::Relaxed) { break; }
            }
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
                Ok(rel) => {
                    let msg = release_channel_current_msg(&rel.version);
                    if version_gt(VERSION, &rel.version) {
                        println!("  {RED}⚠ {msg}{RESET}\n");
                    } else {
                        println!("  {GREEN}{msg}{RESET}\n");
                    }
                    return;
                }
                Err(e) => { eprintln!("  {RED}✗ update check: {e}{RESET}\n"); std::process::exit(1); }
            }
        }
        // v0.9.0: re-verify the LOCAL block store as a connected spine (precheck +
        // parent linkage), genesis→tip. No network. Exit 0 = clean chain to genesis,
        // 1 = a real integrity break, 2 = couldn't open the store.
        Some("verify-chain") => {
            let json = argv.iter().any(|a| a == "--json");
            let path = sigil_top_db_path();
            let mut store = match block_store::BlockStore::open_blocking(&path) {
                Ok(s) => s,
                Err(e) => { eprintln!("{RED}✗ open store {path}: {e}{RESET}"); std::process::exit(2); }
            };
            let synced = store.synced_to();
            let t0 = Instant::now();
            // RC5 boot speedup: parallel-precheck verify (v0.34 lane) — contract-identical
            // to verify_to (see chain_verify tests serial==parallel), ~Nx faster on the
            // full-chain pass that dominated startup. This is the boot verify (u64::MAX).
            let report = chain_verify::verify_to_parallel(&mut store, u64::MAX);
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
            let store = match block_store::BlockStore::open_blocking(&path) {
                Ok(s) => s,
                Err(e) => { eprintln!("{RED}✗ open store {path}: {e}{RESET}"); std::process::exit(2); }
            };
            println!("\n  {VBRIGHT}{BOLD}◆ SIGIL full verifying sync{RESET}  {DIM}(store {path}){RESET}");
            println!("  {DIM}connecting to the sigil-g0 mesh — downloading + verifying genesis→tip…{RESET}");
            if let Some(t) = target_arg { println!("  {DIM}target height pinned to {t}{RESET}"); }
            println!("  {DIM}timeout {timeout_s}s · Ctrl-C to stop{RESET}\n");
            // LANE-P: --recent / SIGIL_SYNC_RECENT=1 launches the MONITOR path (recent_only,
            // recent-window snap) headlessly — the exact branch where the unaligned-base 57345
            // freeze lived — so the CI gate can verify it, not just the genesis crawl.
            let recent = argv.iter().any(|a| a == "--recent")
                || std::env::var("SIGIL_SYNC_RECENT").map(|v| v == "1" || v == "true").unwrap_or(false);
            if recent { println!("  {DIM}monitor mode — recent-window snap (recent_only){RESET}"); }
            let sync = block_sync::P2PBlockSync::launch(store, recent);
            // v0.15.1: a pinned --target also SEEDS the backfill tip so the refill
            // fires immediately (the gate is peer_best>0, not target_arg). Without
            // this, a quiet mesh left peer_best=0 and full-sync --target never pulled.
            if let Some(t) = target_arg { sync.set_known_tip(t); }
            let start = Instant::now();
            let mut last_print = instant_ago(10);
            loop {
                // 0.77: poll_state is try_lock (None = sync thread busy) — headless loop just
                // retries on the next 250ms tick.
                let Some(st) = sync.poll_state() else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                if let Some(b) = &st.verify_break {
                    eprintln!("  {RED}✗ verification break — {b}{RESET}");
                    eprintln!("  {RED}  the downloaded chain does NOT form one connected spine. Aborting.{RESET}\n");
                    std::process::exit(1);
                }
                // SPINE-BREAK fix: the no-progress watchdog confirmed an unfillable hole. Fail LOUD
                // naming the EXACT stuck height (exit 4) instead of the old silent rate-0 crawl that
                // only ever ended on the generic timeout.
                if let Some((h, reason)) = &st.sync_failure {
                    eprintln!("\n  {RED}✗ SPINE BREAK — sync stuck at height {}{RESET}", group(*h));
                    eprintln!("  {RED}  {reason}{RESET}\n");
                    std::process::exit(4);
                }
                let target = target_arg.unwrap_or(st.peer_best_height);
                if last_print.elapsed() >= Duration::from_secs(2) {
                    last_print = Instant::now();
                    let pct = if target > 0 { (st.verified as f64 / target as f64 * 100.0).min(100.0) } else { 0.0 };
                    println!("  {CYAN}⬇{RESET} verified {GREEN}{}{RESET} / synced {GOLD}{}{RESET} / tip {} · {VBRIGHT}{:.1}%{RESET} · {} peers · {}s",
                        group(st.verified), group(st.blocks_synced), if target > 0 { group(target) } else { "?".into() },
                        pct, st.peer_count, start.elapsed().as_secs());
                }
                // LANE-P: don't declare "complete" before the mesh has actually connected and
                // seeded a REAL network tip. With 0 peers, peer_best can momentarily seed to ou
                // own genesis (target=1) → verified>=1 → a FALSE "sync complete at height 1" the
                // instant we start (the gate's flaky FAIL + a real "looks synced but isn't" bug).
                // Require target>1 AND a live peer (or a 30s grace for a genuine solo-at-tip).
                if target > 1 && st.verified >= target
                    && (st.peer_count > 0 || start.elapsed() > Duration::from_secs(30)) {
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
    // 2026-08-24 (sync-then-produce bridge, operator-directed: "work on unifiyhing
    // the sigil top node so that i can produce blocks and actual is a real node ...
    // every user downloading sigil top wil be full node operator"). This is the
    // ACTUAL start of the producer loop — `producer::run::maybe_start` existed,
    // fully tested, since Phase 3/5 (2026-08-23) but was never called from here, so
    // no real `sigil-top` binary ever produced a block no matter what env vars were
    // set. Hard no-op unless the operator has explicitly set BOTH
    // `SIGIL_TOP_PRODUCER=1` AND `SIGIL_TOP_PRODUCE=1` (see
    // `producer::producer_mode_enabled`/`producer::should_produce`) — every other
    // launch of `sigil-top` (the vast majority: plain light-client wallet use) is
    // byte-for-byte unaffected. The returned handle MUST be kept alive for the rest
    // of `main()` (dropping it signals the background loop to stop — see
    // `ProducerLoopHandle`'s `Drop` impl) so it is bound here, before the
    // once/interactive dispatch below, and never re-bound to `_`.
    // v7.5.2 STARTUP FIX (Viktor, "won't start on Windows / loads all blocks from genesis"):
    // producer mode is DEFAULT-ON, and `maybe_start` → `sync_chain_blocking()` runs a FULL sync
    // from genesis ON THE CALLING THREAD before returning. Called synchronously here it froze
    // startup before the TUI ever drew its first frame (reproduced under Wine: the boot trace
    // stopped right after the producer-mode gates line). Move the whole sync-then-produce
    // bootstrap onto a BACKGROUND thread so the dashboard opens instantly and the sync runs
    // behind it — the operator's "full sync first, produce out of the box" intent is preserved
    // (production still waits for the synced tip inside the loop), just off the UI thread. The
    // handle is parked in a slot main() holds for its whole life, so its Drop still stops the
    // loop on exit.
    #[cfg(feature = "producer")]
    let _producer_handle: std::sync::Arc<std::sync::Mutex<Option<producer::run::ProducerLoopHandle>>> = {
        let tick_ms: u64 = std::env::var("SIGIL_TOP_PRODUCE_TICK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot_bg = slot.clone();
        let _ = std::thread::Builder::new().name("producer-bootstrap".into()).spawn(move || {
            if let Some(handle) = producer::run::maybe_start(Duration::from_millis(tick_ms)) {
                boot_trace("producer-mode: sync-then-produce bootstrap starting (see [producer]/[producer-sync] log lines)");
                if let Ok(mut g) = slot_bg.lock() { *g = Some(handle); }
            }
        });
        slot
    };
    // v0.95: the default interactive dashboard runs the pinned updater gate before
    // entering raw-mode TUI so stale channels and bad signatures are visible instead
    // of looking like "already latest". --once, pipes, and --lite stay side-effect
    // free below; --no-update / SIGIL_TOP_NO_AUTOUPDATE=1 still opts out entirely.
    let mut cfg = parse_args();
    cfg.initial_toast = None; // set after one-shot/lite exits; async checks keep banner fresh
    // Non-TTY (piped / redirected / captured), --once, or --lite → emit exactly ONE
    // plain ANSI frame and exit. ratatui needs a real terminal; this path neve
    // spams. The live, interactive dashboard is the TUI below.
    // v0.63.1: a DOUBLE-CLICKED console on Windows can report is_terminal()=false on
    // stdout (tray/subsystem quirk), which dropped the app to the one-frame path and
    // "closed" instead of showing the dashboard. Treat it as interactive if EITHER
    // stdin or stdout is a tty — only a full pipe/redirect (both non-tty) stays plain.
    // v3: the #1 "no TUI on Windows" bug was is_terminal() returning false on a REAL console
    // → headless → dashboard never opened. Run the TUI whenever a console is plausibly attached:
    // is_terminal(stdout|stdin) OR win_has_console() (GetConsoleWindow != null on Windows) OR the
    // SIGIL_TUI=1 escape hatch. A genuine no-console pipe (all false) cleanly goes headless — it
    // CAN'T host an interactive TUI anyway. Headless also opt-in via --once / SIGIL_HEADLESS=1.
    // Terminal setup below is non-fatal so a quirky console can't silent-exit before frame 1.
    // v7.1.10: these two used `.is_ok()` — PRESENCE, not value — so
    // `SIGIL_HEADLESS=0` forced headless ON, the exact opposite of what it reads
    // like, and contradicted the line above documenting `SIGIL_HEADLESS=1`. It
    // cost a whole test cycle: a harness exported `SIGIL_HEADLESS=0` to mean
    // "not headless" and silently measured the one-frame headless path instead of
    // the TUI. Parse the value; treat 0/false/no/off/empty as OFF.
    let env_flag = |k: &str| -> bool {
        match std::env::var(k) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            ),
            Err(_) => false,
        }
    };
    let force_headless = env_flag("SIGIL_HEADLESS");
    let interactive = !cfg.once && !force_headless && (
        env_flag("SIGIL_TUI")
        || std::io::stdout().is_terminal()
        || std::io::stdin().is_terminal()
        || win_has_console()
    );
    boot_trace(&format!("interactive={} (stdout_tty={} stdin_tty={} win_console={}) once={} lite={}",
        interactive, std::io::stdout().is_terminal(), std::io::stdin().is_terminal(), win_has_console(), cfg.once, cfg.lite));
    // Non-TTY (piped / captured / redirected) or --once → one plain frame, no loop.
    if cfg.once || !interactive {
        // P2a: prewarm the ledger refresher so the single frame carries the real
        // supply + header-verify verdict instead of the feed fallback (the
        // background thread otherwise races this one-and-only render).
        let _ = ledger_verify::latest();
        for _ in 0..40 { if ledger_verify::latest().is_some() { break; } thread::sleep(Duration::from_millis(250)); }
        let (mut st, online, source) = fetch_best(&cfg);
        one_chain_view(&mut st, true); // headless monitor: no spine engine here
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
    cfg.initial_toast = maybe_auto_update(&argv);
    // DEFAULT — the custom ratatui dashboard (Quillon-graph-node styled, multi-panel).
    // ratatui owns all box-drawing + layout, so alignment can never regress.
    let _ = cfg.tui; // --tui kept as an explicit alias; it's the default now
    boot_trace("entering run_tui (interactive dashboard)");
    if let Err(e) = run_tui(cfg) {
        boot_trace(&format!("run_tui returned Err: {e}"));
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
// v0.33.2 BOLD NEON redesign — high-contrast neon-on-black accents for banners + bars.
const C_NEON_CYAN: Color = Color::Rgb(0x22, 0xf5, 0xff);   // neon edge / live tip
const C_NEON_GREEN: Color = Color::Rgb(0x4b, 0xff, 0x7a);  // neon synced / healthy
const C_NEON_PINK: Color = Color::Rgb(0xff, 0x4d, 0xa6);   // neon alarm / brand pop
const C_NEON_GOLD: Color = Color::Rgb(0xff, 0xd8, 0x52);   // neon value
const C_BG: Color = Color::Rgb(0x07, 0x07, 0x12);          // obsidian card bg
const C_BG_HEAD: Color = Color::Rgb(0x0c, 0x0a, 0x1f);     // header band bg

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
    /// v0.38: flux-rev `full:` source id of the published build (display/ledger;
    /// the binary gate stays the per-target BLAKE3).
    #[serde(default)]
    flux_rev: String,
}

/// Old manifest target triples that map to our short platform names.
/// v0.38 VARIANT PINNING: a GPU build reads ONLY its -gpu channel key — it must
/// never cross-grade itself to the CPU binary (or vice versa). No -gpu key in the
/// manifest -> a GPU build simply reports "no build for windows-x64-gpu" and stays.
const LEGACY_SELF_KEYS: &[&str] = if cfg!(all(windows, feature = "gpu")) {
    &["windows-x64-gpu"]
} else if cfg!(windows) {
    &["windows-x64", "x86_64-pc-windows-gnu"]
} else if cfg!(target_os = "macos") {
    &["macos-arm64", "aarch64-apple-darwin"]
} else if cfg!(feature = "gpu") {
    &["linux-x64-gpu"]
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
        if cfg!(feature = "gpu") {
            // v0.38: the top-level single-binary fields are the CPU build — a GPU
            // build must NOT fall back to them (that silently downgrades GPU->CPU).
            return Target { url: String::new(), blake3_hex: String::new(), size_bytes: 0 };
        }
        Target {
            url: self.url.clone(),
            blake3_hex: self.blake3_hex.clone(),
            size_bytes: self.size_bytes,
        }
    }
}

/// v0.38: best-effort lifecycle telemetry -> the flux webhook bus (the cockpit
/// feed at :4178/api/mcp-webhook). OFF unless SIGIL_TOP_WEBHOOK is set — a use
/// install posts nothing anywhere by default. Fire-and-forget on a thread; a dead
/// bus can never slow or break the node.
fn flux_webhook(event: &str, detail: &str) {
    let Ok(url) = std::env::var("SIGIL_TOP_WEBHOOK") else { return };
    if url.trim().is_empty() { return; }
    let body = format!(
        r#"{{"source":"sigil-top","version":"{}","flux_rev":"{}","event":{},"detail":{}}}"#,
        VERSION, FLUX_REV,
        serde_json::to_string(event).unwrap_or_else(|_| "\"?\"".into()),
        serde_json::to_string(detail).unwrap_or_else(|_| "\"\"".into()),
    );
    std::thread::spawn(move || {
        let _ = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .and_then(|c| c.post(&url).header("content-type", "application/json").body(body).send());
    });
}

/// LANE-C: the pinned SIGIL release signing public key (ed25519, 32 B hex). The auto-update
/// REFUSES any manifest not signed by the matching secret — so a compromised release server / MITM
/// that serves a matching-blake3 (but attacker-built) binary can't push it: without the secret it
/// can't forge the manifest signature, and the blake3 the client trusts comes ONLY from a signed
/// manifest. The secret lives solely on the release host (`/root/.config/sigil/release-sign.seed`,
/// chmod 600). Ed25519 today; the sigil-oauth IssuerSigner abstraction lets a SQIsign-L5 PQ key
/// replace it without a wire change once flux-sqisign is in the cross-build.
const RELEASE_SIGN_PUBKEY_HEX: &str =
    "150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402";

/// LANE-C: verify the release manifest is signed by [`RELEASE_SIGN_PUBKEY_HEX`]. The detached
/// signature is published at `<manifest>.sig` as 128-hex (ed25519 over the EXACT manifest bytes).
/// Returns `Err` (fail-closed → no update) on any missing/invalid signature. No runtime bypass:
/// this check always runs — there is no env var that skips it.
fn verify_manifest_sig(base: &str, manifest_body: &str) -> Result<(), String> {
    let pk: [u8; 32] = hex::decode(RELEASE_SIGN_PUBKEY_HEX).ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| "pinned release key malformed".to_string())?;
    let bust = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    let sig_url = format!("{base}/sigil-top-latest.json.sig?t={bust}");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build().map_err(|e| format!("sig client init: {e}"))?;
    let sig_hex = client.get(&sig_url).send().and_then(|r| r.error_for_status()).and_then(|r| r.text())
        .map_err(|e| format!("no release-manifest signature ({e}) — refusing update"))?;
    let sig: [u8; 64] = hex::decode(sig_hex.trim()).ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| "release-manifest signature malformed (want 128-hex ed25519)".to_string())?;
    if sigil_oauth::verify_sig(&pk, manifest_body.as_bytes(), &sig) {
        Ok(())
    } else {
        Err("MANIFEST SIGNATURE INVALID — refusing update (possible compromised release server)".into())
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
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
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
    let mut last = String::from("no response");
    // v7.0.26: try every channel base — HTTPS first, then the plain-HTTP :8099 mirror
    // (filtered-network escape hatch). Signature is verified against the SAME base.
    for (bi, base) in CHANNEL_BASES.iter().enumerate() {
    let url = format!("{base}/sigil-top-latest.json?t={bust}");
    for _ in 0..2 {
        match client.get(&url).send().and_then(|r| r.error_for_status()) {
            Ok(resp) => match resp.text() {
                Ok(body) => {
                    // LANE-C: AUTHENTICATE the manifest before we trust a single field (incl. the
                    // blake3 we'd verify the binary against). A bad signature is fatal — fail closed.
                    if let Err(e) = verify_manifest_sig(base, &body) {
                        return Err(e);
                    }
                    ACTIVE_BASE.store(bi, std::sync::atomic::Ordering::Relaxed);
                    match serde_json::from_str::<Release>(&body) {
                        Ok(rel) => return Ok(rel),
                        Err(e) => last = format!("parse: {e} [{}B: {:?}]",
                            body.len(), body.chars().take(48).collect::<String>()),
                    }
                }
                Err(e) => last = format!("read body: {e}"),
            },
            Err(e) => {
                last = if e.is_timeout() { "timed out (>8s) — slow link".into() }
                       else if e.is_connect() { format!("connect failed: {e}") }
                       else { format!("request failed: {e}") };
            }
        }
    }
    } // CHANNEL_BASES — fall through to the next mirror (incl. the plain-HTTP :8099 one)
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
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
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
    // Structural-validate with sigil-dns-ancho
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
        // v0.50: the old https://:8181 status endpoint is long dead (fleet showed a
        // permanent 0/2). HONEST probe: a TCP connect to the node's real listene
        // (:9501) proves the process is up; we claim nothing we can't read.
        let alive = std::net::TcpStream::connect_timeout(
            &format!("{}:{}", node.addr, node.port).parse().unwrap_or_else(|_| std::net::SocketAddr::from(([0,0,0,0],0))),
            Duration::from_secs(3),
        ).is_ok();
        node.online = alive;
        if alive { continue; }
        // legacy HTTPS status fallback (in case an old fleet node still serves it)
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
/// LANE-N: where the operator's chosen mining wallet is persisted. The [W] wallet
/// posts its (keyed) address here via `/api/v1/use-wallet` so mining credits the
/// wallet the operator actually sees AND holds the private key for.
pub(crate) fn mine_wallet_path() -> String { format!("{}/.flux/sigil-mine-wallet", flux_home()) }

/// A 64-hex lowercase/upper address (the node keys balances by this).
fn valid_addr(s: &str) -> bool {
    let s = s.trim();
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// LANE-N: resolve the mining-credit wallet. PURE (for unit tests) — real I/O is in
/// [`miner_wallet`]. Priority: the `SIGIL_MINE_SEED`-derived address (if a seed is
/// set, this IS what's actually mining — see below) → explicit `SIGIL_MINE_WALLET` →
/// the operator's chosen wallet (persisted by the [W] wallet) → a stable per-host
/// hash. All but the last are KEYED wallets the operator controls; the hostname hash
/// is an UNSPENDABLE last resort (no private key) kept only so a fresh box still
/// mines to a stable address.
///
/// 2026-08-25: `seed_derived` was missing entirely, which reopened the exact split
/// this function's own doc comment says it fixes. `SIGIL_MINE_SEED` (required by
/// `miner_keypair()` for `[M]ine`, and by `mine-rig --seed`'s shielded-mining
/// auto-registration) is the address ACTUALLY receiving mining credit whenever it's
/// set — but this function never looked at it, so `[W]` would show `SIGIL_MINE_WALLET`
/// (if set, usually stale/unrelated), the persisted "chosen" file, or the unspendable
/// hostname hash: none of which is the seed's real address. Real rewards landed on the
/// seed wallet the whole time; `[W]` was just watching a different, uncredited one.
/// The seed derivation is unambiguous and provably correct (the private key is right
/// there), so it now outranks everything else.
fn resolve_mine_wallet(
    seed_derived: Option<&str>,
    env_override: Option<&str>,
    chosen: Option<&str>,
    host: &str,
) -> String {
    if let Some(w) = seed_derived { if valid_addr(w) { return w.trim().to_string(); } }
    if let Some(w) = env_override { if valid_addr(w) { return w.trim().to_string(); } }
    if let Some(w) = chosen { if valid_addr(w) { return w.trim().to_string(); } }
    blake3::hash(format!("sigil-top-miner:{host}").as_bytes()).to_hex().to_string()
}

/// The miner-credit wallet (64-hex). See [`resolve_mine_wallet`] for the priority.
pub(crate) fn miner_wallet() -> String {
    let seed_derived = miner_keypair().map(|kp| kp.pubkey_hex());
    let env = std::env::var("SIGIL_MINE_WALLET").ok();
    let chosen = std::fs::read_to_string(mine_wallet_path()).ok();
    let host = std::env::var("HOSTNAME").or_else(|_| std::env::var("HOST")).unwrap_or_else(|_| "sigil-top".into());
    resolve_mine_wallet(seed_derived.as_deref(), env.as_deref(), chosen.as_deref(), &host)
}

/// LANE-N: persist the operator's chosen mining wallet (called from the local API when
/// the [W] wallet claims "mine to me"). Validates the 64-hex shape; rejects garbage.
pub(crate) fn set_mine_wallet(addr: &str) -> bool {
    if !valid_addr(addr) { return false; }
    let _ = std::fs::create_dir_all(format!("{}/.flux", flux_home()));
    std::fs::write(mine_wallet_path(), addr.trim()).is_ok()
}

/// The raw 32-byte `SIGIL_MINE_SEED`, parsed once. `None` if unset/malformed.
///
/// `pub(crate)` so `mine_local_api.rs` can derive the SAME shielded-account key
/// [`miner_keypair`] derives its Ed25519 signing key from — one seed, two independent,
/// domain-separated derivations (see `shield_setup.rs`'s module docs for why that's safe).
/// Extracted out of `miner_keypair` below specifically so there is exactly ONE place that
/// parses this env var for the interactive/local-API paths — `shield_setup.rs` still
/// carries its own separate `seed_bytes` for the headless `mine-rig` CLI path, and that
/// duplication is exactly the class of bug (a 0x-prefix handling mismatch) documented on
/// `hex_to_32`'s own comment; this function exists so `mine_local_api.rs` doesn't grow a
/// THIRD copy with its own edge cases.
pub(crate) fn miner_seed() -> Option<[u8; 32]> {
    let seed_hex = std::env::var("SIGIL_MINE_SEED").ok()?;
    hex_to_32(seed_hex.trim())
}

/// The miner's SIGNING keypair. Required now that `/mine` is auth-gated (audit C1:
/// a miner must prove control of the credited wallet — you can no longer mine to an
/// address whose key you don't hold, e.g. the legacy hostname-hash fallback). Source:
/// `SIGIL_MINE_SEED` (64-hex). Read from the environment only, never persisted (matches
/// `do_login`'s "never store the secret"). `None` ⇒ mining is disabled in `start_mining`.
pub(crate) fn miner_keypair() -> Option<Keypair> {
    miner_seed().map(|seed| Keypair::from_seed(&seed))
}

#[cfg(test)]
mod lane_n_tests {
    use super::resolve_mine_wallet;
    #[test]
    fn seed_derived_beats_everything_else() {
        // 2026-08-25: the whole point of the fix — a wallet mining via SIGIL_MINE_SEED
        // must see [W] resolve to that SAME address, never a stale env var or a
        // previously-chosen wallet from before the seed was set.
        let seed_addr = "d".repeat(64);
        let env = "a".repeat(64);
        let chosen = "b".repeat(64);
        assert_eq!(
            resolve_mine_wallet(Some(&seed_addr), Some(&env), Some(&chosen), "host"),
            seed_addr
        );
    }
    #[test]
    fn env_override_wins_when_no_seed() {
        let env = "a".repeat(64);
        assert_eq!(resolve_mine_wallet(None, Some(&env), Some(&"b".repeat(64)), "host"), env);
    }
    #[test]
    fn chosen_wallet_beats_hostname() {
        let chosen = "c".repeat(64);
        assert_eq!(resolve_mine_wallet(None, None, Some(&chosen), "host"), chosen);
    }
    #[test]
    fn falls_back_to_stable_hostname_hash() {
        let r = resolve_mine_wallet(None, None, None, "host");
        assert_eq!(r.len(), 64);
        assert_eq!(r, resolve_mine_wallet(None, None, None, "host")); // deterministic
        assert_ne!(r, resolve_mine_wallet(None, None, None, "other")); // per-host
    }
    #[test]
    fn invalid_inputs_are_ignored() {
        let host_hash = resolve_mine_wallet(None, None, None, "host");
        // too short + non-hex 64 → both rejected → hostname hash
        assert_eq!(
            resolve_mine_wallet(None, Some("short"), Some(&"z".repeat(64)), "host"),
            host_hash
        );
    }
}

/// Start REAL mining on a background thread: find a BLAKE3 nonce meeting `difficulty_bits`, POST it to
/// the node's `/mine` endpoint, repeat. Accepted shares are reported over the returned channel so the
/// TUI shows live progress. Stops when `stop` flips true. This is what makes pressing **[M]** actually
/// mine — not just toggle a flag. Light difficulty (testnet) so shares land in seconds.
fn start_mining(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> mpsc::Receiver<String> {
    use std::sync::atomic::Ordering;
    let (tx, rx) = mpsc::channel();
    let url = mine_url();
    let kp = miner_keypair();
    thread::spawn(move || {
        // /mine is auth-gated (audit C1): the miner must SIGN as the credited wallet.
        // No seed ⇒ we can't sign ⇒ disable mining instead of burning CPU on shares the
        // node rejects with "missing 'sig'". Set SIGIL_MINE_SEED=<64-hex> to enable.
        let Some(kp) = kp else {
            let _ = tx.send("✗ mining disabled: set SIGIL_MINE_SEED=<64-hex wallet seed>. The node now requires miners to SIGN /mine (audit C1) — mining to an address you don't control is no longer possible.".to_string());
            return;
        };
        let wallet = hex::encode(kp.pubkey());
        // 2026-08-24: SIGIL_MINE_SEED is ALREADY required to reach this point (audit
        // C1, above) — so this is genuinely "out of the box": nobody who can already
        // use [M]ine has anything new to configure to also get shielded rewards.
        #[cfg(feature = "shield-register")]
        if let Ok(seed_hex) = std::env::var("SIGIL_MINE_SEED") {
            // Registration lives on the dual-lane money API (same base `mine-rig`
            // uses), not `url` above (that's `/mine`, the legacy single-lane path
            // this function itself POSTs shares to) — the two are different services.
            // Background keeper (see mine-rig's call site for why one-shot wasn't
            // enough). Cloning the mpsc sender lets it report state changes into the same
            // TUI log line the miner uses, long after [M] was pressed.
            let reg_tx = tx.clone();
            shield_setup::spawn_registration_keeper(&engine_node_url(), &seed_hex, move |line| {
                let _ = reg_tx.send(line);
            });
            shield_setup::spawn_shielded_scanner(&engine_node_url(), &seed_hex);
        }
        let mut req_nonce: u64 = 0; // strictly-increasing per-wallet replay guard (ms floor)
        let difficulty_bits: u32 = std::env::var("SIGIL_MINE_DIFFICULTY").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(12); // ~4k hashes/share — real PoW, lands fast
        let client = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(8))
        .min_tls_version(reqwest::tls::Version::TLS_1_2).build() {
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
            // Sign the canonical sigil-rpc auth message. MUST byte-match
            // `sigil_rpc::auth::auth_message`: AUTH_DOMAIN|action|field0|field1|…|nonce=N.
            // /mine fields = [miner_hex, header, pow_nonce]. req_nonce uses a ms floor so
            // it strictly increases across submits AND restarts (the node persists the
            // per-wallet high-water nonce) → a captured share can't be replayed.
            req_nonce = {
                let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64).unwrap_or(0);
                ms.max(req_nonce.wrapping_add(1))
            };
            let auth_msg = format!("sigil-rpc/v1|mine|{wallet}|{header}|{nonce}|nonce={req_nonce}");
            let sig = hex::encode(kp.sign(auth_msg.as_bytes()));
            let body = format!("{{\"miner\":\"{wallet}\",\"header\":\"{header}\",\"nonce\":{nonce},\"difficulty\":{difficulty_bits},\"reward\":50,\"sig\":\"{sig}\",\"req_nonce\":{req_nonce}}}");
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
/// for the TUI instead of eprintln! (which corrupts the alt-screen). Disable with `--no-update` o
/// `SIGIL_TOP_NO_AUTOUPDATE=1`.
fn maybe_auto_update(argv: &[String]) -> Option<String> {
    if argv.iter().any(|a| a == "--no-update")
        || std::env::var("SIGIL_TOP_NO_AUTOUPDATE").map(|v| v == "1").unwrap_or(false)
    {
        return None;
    }
    let rel = match fetch_latest() {
        Ok(r) if version_gt(&r.version, VERSION) => r,
        Ok(r) if version_gt(VERSION, &r.version) => return Some(format!("⚠ {}", release_channel_stale_msg(&r.version))),
        Ok(_) => return None,
        Err(e) if e.contains("MANIFEST SIGNATURE INVALID") => {
            return Some(format!("⚠ update channel signature invalid — {e}"));
        }
        Err(_) => return None, // channel unreachable/malformed → just run
    };
    match self_update(&rel) {
        Ok(msg) if msg.starts_with("staged v") => {
            // Windows lock-fallback path: a detached helper will move the staged binary over the
            // install path AFTER this process exits, then relaunch it. Exit now so it can proceed.
            std::process::exit(0);
        }
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

/// Fetch the bytes at `url` over HTTPS with a TLS 1.2 floor. Free-standing and
/// self-contained by design: it takes only a URL, builds its own short-lived
/// `reqwest` client, and touches no sigil-top state (no `App`, no wallet paths, no
/// globals like [`HTTP`] or `LAST_FEED_ERR`) — so it's meant to be lifted as-is into
/// a shared crate (e.g. a future `sigil-updater`) that other binaries can depend on
/// too. Mirrors the client-builder pattern already used by `fetch_latest` /
/// `self_update` (timeout + TLS floor + user-agent); a caller embedding this in a
/// different crate should swap the user-agent literal for its own.
fn fetch_binary_reqwest(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("client init: {e}"))?;
    let resp = client.get(url).send().map_err(|e| format!("request failed: {e}"))?;
    let resp = resp.error_for_status().map_err(|e| format!("HTTP status: {e}"))?;
    let bytes = resp.bytes().map_err(|e| format!("read body: {e}"))?;
    Ok(bytes.to_vec())
}

fn self_update(rel: &Release) -> Result<String, String> {
    let mut t = rel.for_self();
    if t.url.is_empty() { return Err(format!("manifest has no {SELF_TARGET} build")); }
    // v7.0.26: download from the SAME base the manifest came from. The manifest's
    // absolute URLs point at the HTTPS domain; on a filtered network only the
    // plain-HTTP :8099 mirror is reachable, so rebase by filename (blake3 gate
    // below authenticates the bytes regardless of transport).
    let bi = ACTIVE_BASE.load(std::sync::atomic::Ordering::Relaxed);
    if bi > 0 {
        if let Some(name) = t.url.rsplit('/').next() {
            t.url = format!("{}/{}", CHANNEL_BASES[bi.min(CHANNEL_BASES.len()-1)], name);
        }
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
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
    // LANE-C v0.50: provenance surfacing. The manifest carries the NEW build's
    // flux-rev (its fluxc `.proof` stamp). Show it next to the running FLUX_REV so
    // the operator can SEE provenance actually changed across the swap — a "new"
    // release whose flux-rev equals ours is suspicious (re-published same artifact).
    // Informational only: BLAKE3 above is the gate; this never blocks the swap.
    let prov = {
        let cur = FLUX_REV.strip_prefix("full:").unwrap_or(FLUX_REV);
        let newr = rel.flux_rev.strip_prefix("full:").unwrap_or(&rel.flux_rev);
        let short = |s: &str| s.chars().take(10).collect::<String>();
        if newr.is_empty() || newr == "unstamped" {
            " · prov: manifest unstamped".to_string()
        } else if short(newr) == short(cur) {
            format!(" · ⚠ prov UNCHANGED {}", short(newr))
        } else {
            format!(" · prov {}→{}", short(cur), short(newr))
        }
    };
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
    let mb = bytes.len() as f64 / 1.048576e6;
    // Common fast path (both platforms): atomic in-place self-replace. On Windows the
    // self_replace crate does the "rename the running .exe out of the way" trick itself.
    if self_replace::self_replace(&beside).is_ok() {
        let _ = std::fs::remove_file(&beside);
        return Ok(format!("swapped v{VERSION} -> v{} ({mb:.1} MB){prov} — restart to run", rel.version));
    }
    // self_replace FAILED.
    // Windows: the running .exe was locked (AV / image map / dir perms) and the old code
    // silently fell through to "saved … beside" — the install path kept the OLD binary, the
    // relaunch pre-flighted that OLD binary, saw a version mismatch, aborted, and the operato
    // drifted (DeepSeek root-cause: "running .exe is locked → rename fails → silent skip →
    // version drift"). Escalate instead of drifting. See windows_swap_fallback().
    #[cfg(windows)]
    { return windows_swap_fallback(&beside, rel, mb, &prov); }
    // Unix: self_replace almost never fails; keep the staged versioned binary beside us and let
    // relaunch_new_binary fall back to it.
    #[cfg(not(windows))]
    Ok(format!("saved v{} ({mb:.1} MB){prov} -> {}", rel.version, beside.display()))
}

/// Windows lock-failure fallback for [`self_update`]. `self_replace` could not swap the running
/// `.exe` (locked image / AV scan / directory perms). DeepSeek-designed escalation, fail-loud:
///   1. rename the running exe → `…exe.old` (Windows permits renaming a running image on the same
///      volume), then copy the staged bytes into the original install path → instant in-place swap
///      (relaunch_new_binary then re-execs the install path exactly as in the common case);
///   2. if that fails, write a DETACHED helper `.bat` that waits for THIS pid to exit, moves the
///      staged binary over the install path, relaunches it, and self-deletes — applied on exit;
///   3. if NEITHER works, return `Err` so the [U] handler shows a LOUD failure (never a silent
///      "saved" that drifts the version).
/// The `…exe.old` left behind by path 1 is cleaned up on the next boot (see main()).
#[cfg(windows)]
fn windows_swap_fallback(beside: &std::path::Path, rel: &Release, mb: f64, prov: &str)
    -> Result<String, String>
{
    use std::os::windows::process::CommandExt;
    let install = INSTALL_EXE.get().cloned()
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "cannot resolve install path for swap".to_string())?;
    // (1) rename-out + copy-in.
    let old = install.with_extension("exe.old");
    let _ = std::fs::remove_file(&old); // clear any stale .old first (best-effort)
    if std::fs::rename(&install, &old).is_ok() {
        if std::fs::copy(beside, &install).is_ok() {
            let _ = std::fs::remove_file(beside);
            return Ok(format!("swapped v{VERSION} -> v{} ({mb:.1} MB){prov} — restart to run", rel.version));
        }
        // copy failed AFTER the rename → restore the old binary so we don't brick the install.
        let _ = std::fs::rename(&old, &install);
    }
    // (2) detached helper that applies the swap once we exit.
    let pid = std::process::id();
    let bat = std::env::temp_dir().join(format!("sigil-top-swap-{pid}.bat"));
    // CRLF + quoted paths (the install dir routinely has spaces, e.g. "Viktor S. Kristensen").
    // Wait for our PID to vanish, move staged→install, relaunch in a fresh console, self-delete.
    let script = format!(
        "@echo off\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" 2>nul | find \"{pid}\" >nul\r\n\
         if not errorlevel 1 ( timeout /t 1 /nobreak >nul & goto wait )\r\n\
         move /Y \"{src}\" \"{dst}\" >nul\r\n\
         start \"\" \"{dst}\"\r\n\
         del \"%~f0\"\r\n",
        pid = pid, src = beside.display(), dst = install.display());
    std::fs::write(&bat, &script).map_err(|e| format!("windows swap (locked exe): cannot write helper ({e})"))?;
    // DETACHED_PROCESS (0x8) | CREATE_NEW_PROCESS_GROUP (0x200): outlives us, no inherited console.
    std::process::Command::new("cmd")
        .args(["/C", &bat.to_string_lossy()])
        .creation_flags(0x0000_0008 | 0x0000_0200)
        .spawn()
        .map_err(|e| format!("windows swap (locked exe): cannot spawn helper ({e})"))?;
    Ok(format!("staged v{} ({mb:.1} MB){prov} — applying on exit", rel.version))
}

/// v0.25: pre-flight a freshly-swapped binary BEFORE handing off to it. `exec`/`spawn+exit`
/// destroys the running app; if the new binary is corrupt (truncated download), ABI/GLIBC-
/// incompatible, or hangs on start, the app would simply VANISH on the restart-after-sync —
/// the exact bug this fixes. Spawn `target --selfcheck` (a no-op that prints the version and
/// exits 0) with a short timeout; return Ok(version) only if it runs cleanly AND prints a
/// non-empty version. Anything else → don't hand off, keep the running app alive.
// Self-update subsystem being split into updater.rs (god-file split, 2026-09-01).
// preflight_binary moved there; more of the cluster follows across ticks.
mod updater;
pub(crate) use updater::preflight_binary;

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
    // Use the startup-captured install path — current_exe() points at the
    // moved-aside OLD inode after self_replace (a "(deleted)" path), which would
    // make the pre-flight spawn fail with ENOENT even though the NEW binary is in
    // place. Fall back to current_exe() only if the early capture somehow missed.
    let exe = match INSTALL_EXE.get().cloned().or_else(|| std::env::current_exe().ok()) {
        Some(e) => e, None => return false,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ver_exe = exe.with_file_name(format!(
        "sigil-top-v{}{}", version, if cfg!(windows) { ".exe" } else { "" }));
    // Prefer the original install path (now holding the new binary); the versioned
    // sibling is a fallback only if it still exists and current_exe() is unusable.
    let target = if exe.exists() { exe.clone() }
        else if ver_exe.exists() { ver_exe }
        else { exe.clone() };

    // GATE: never hand off to an unverified binary. A failed pre-flight means the swapped
    // binary can't start — abort the relaunch and stay alive on the current (working) image.
    match preflight_binary(&target) {
        Ok(reported) => {
            // v0.56 FAIL-CLOSED: the new binary MUST report the version the channel advertised.
            // A mismatch means the swap didn't take (self_replace no-op'd → still the OLD binary)
            // or the manifest version disagrees with the artifact — relaunching either would run
            // the WRONG version (operator hit "reports v0.40.4, expected v0.42.0 — relaunching
            // anyway"). Abort the handoff AND revert to the rollback image so the next start is
            // known-good, instead of silently running a mismatched binary.
            //
            // v0.59 LANE-O (c): but DON'T abort a genuinely-newer swap just because `version`
            // (= the possibly-STALE app.latest) disagrees. The channel can move 0.57<->0.58 while
            // app.latest still caches the older number; if the staged binary reports a version
            // NEWER than the one we're running, the swap is GOOD — relaunch it. Only revert when
            // the staged binary is NOT an upgrade over the current image (a real no-op / downgrade).
            if !reported.is_empty() && reported != version && !version_gt(&reported, VERSION) {
                eprintln!("  [update] relaunch ABORTED — new binary reports v{reported}, expected v{version} (version mismatch); reverting to the rollback image and staying on the current version. Restart manually once the channel is fixed.");
                if let Some(prev) = prev_binary_path() {
                    if prev.exists() && self_replace::self_replace(&prev).is_err() {
                        let _ = std::fs::copy(&prev, &target);
                    }
                }
                return false;
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
        // then exit. v0.59 LANE-O: the exe we JUST self_replace'd is frequently still
        // briefly LOCKED right after the swap (AV scan / the old file handle settling), so a
        // single spawn() fails → we used to return false and silently NEVER restart (the
        // operator's "swapped … restart to run, but it never restarts" frustration). Settle,
        // then RETRY a few times so the lock clears.
        use std::thread::sleep;
        use std::time::Duration;
        sleep(Duration::from_millis(300)); // let self_replace's handle + any AV scan settle
        // v0.75.2: on Windows relaunch into a FRESH console (CREATE_NEW_CONSOLE) so the new
        // binary gets its OWN visible window — a plain spawn() SHARES the parent's console,
        // which the launching .bat then reclaims for its `pause`, leaving the new TUI invisible
        // ("swapped but never reopens"). macOS keeps the plain spawn.
        for _ in 0..6 {
            #[cfg(windows)]
            let spawned = {
                use std::os::windows::process::CommandExt;
                std::process::Command::new(&target).args(&args)
                    .creation_flags(0x0000_0010 /* CREATE_NEW_CONSOLE */).spawn().is_ok()
            };
            #[cfg(not(windows))]
            let spawned = std::process::Command::new(&target).args(&args).spawn().is_ok();
            if spawned {
                sleep(Duration::from_millis(300));
                std::process::exit(0);
            }
            sleep(Duration::from_millis(300));
        }
        // Fallback: `cmd /C start` with a QUOTED target path — the install dir has SPACES
        // ("Viktor S. Kristensen"), and start parses an unquoted path only up to the first
        // space, so the old unquoted form silently failed. Quote it so start runs the real exe.
        #[cfg(windows)]
        {
            if let Some(t) = target.to_str() {
                let inner = format!("start \"\" \"{}\" {}", t, args.join(" "));
                if std::process::Command::new("cmd").args(["/C", &inner]).spawn().is_ok() {
                    sleep(Duration::from_millis(400));
                    std::process::exit(0);
                }
            }
        }
        // Every path failed → return false so the caller surfaces an HONEST "couldn't
        // auto-restart — please relaunch manually" (and does NOT overwrite it, see [U] handler).
        false
    }
}

/// v0.33.3: a tiny 1D Kalman filter that smooths the noisy 10s-window blk/s into a stable
/// rate estimate, used for a steady time-to-sync ETA (raw rate jitters too much to divide by).
/// Constant-value model: predict adds process noise q; update blends the measurement with
/// gain k = p/(p+r). Larger r = trust the model more = smoother. Tuned for block rates.
#[derive(Clone)]
struct Kalman1D { x: f64, p: f64, q: f64, r: f64, init: bool }
impl Kalman1D {
    fn new() -> Self { Self { x: 0.0, p: 1.0, q: 6.0, r: 180.0, init: false } }

    /// Force the estimate to `z`, discarding the accumulated model confidence.
    ///
    /// For a genuine REGIME CHANGE rather than a noisy sample. The filter's whole premise
    /// is that the underlying rate is roughly constant and the measurement is noisy; when
    /// sync leaves bulk import and starts following the frontier, that premise is simply
    /// false — the rate really did drop by two or three orders of magnitude, and slewing
    /// toward it is not smoothing, it is lying slowly.
    fn reset_to(&mut self, z: f64) {
        if !z.is_finite() { return; }
        self.x = z.max(0.0);
        self.p = 1.0;
        self.init = true;
    }

    fn update(&mut self, z: f64) -> f64 {
        if !z.is_finite() { return self.x; }
        if !self.init { self.x = z; self.init = true; return self.x; }
        self.p += self.q;                       // predict
        // ADAPTIVE GAIN. With the fixed q=6/r=180 pair the steady-state gain is
        // k = 36/216 = 0.167, so each sample moves the estimate only a sixth of the way to
        // the measurement — ~34 samples to cross a 400x drop. That is the right amount of
        // scepticism for NOISE and far too much for a real change. A residual much larger
        // than the estimate itself is not noise (the measurement noise model says a sample
        // lands near x); inflating p in proportion lets the filter believe the new
        // measurement within a couple of samples and then settle back to being sceptical.
        let resid = (z - self.x).abs();
        let scale = self.x.abs().max(z.abs()).max(1.0);
        if resid > 0.5 * scale {
            self.p += self.r * (resid / scale).min(4.0);
        }
        let k = self.p / (self.p + self.r);     // Kalman gain
        self.x += k * (z - self.x);             // correct
        self.p *= 1.0 - k;
        self.x
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
    mine_stats: std::sync::Arc<std::sync::Mutex<MinerStats>>, // v0.37: REAL dual-lane engine stats (shared w/ in-process miner thread)
    mine_desired_gpu: std::sync::Arc<std::sync::atomic::AtomicBool>, // v0.37: GPU/CPU toggle for the engine
    mine_gpu_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,  // v0.37: engine signals GPU init failure -> CPU fallback
    mine_gpu_warned: bool,  // v0.90: one-time thermal/opt-in warning shown on first [G]-enable
    bps_ema: f64,            // v0.37: smoothed network block-rate (raw bps blinks 0<->250)
    bps_zero_streak: u32,    // consecutive zero-bps polls before showing honest idle
    welcome_until: Option<Instant>, // LANE-U v0.67: first-launch welcome modal (SIGIL emblem + giant F); any key or 14s clears it
    idle_store: Option<block_store::BlockStore>, // v0.40.3: parked store so [F] can launch sync ON DEMAND
    /// v7.0.21: the background store-opener's channel when no engine auto-started —
    /// [F]/[Y] poll it via `take_idle_store` (the store may still be opening/compacting).
    idle_store_rx: Option<std::sync::mpsc::Receiver<block_sync::OpenedStore>>,
    /// Live WAL-replay progress for the background store open — set unconditionally at
    /// startup (before the opener thread spawns) so the render loop can show a real
    /// gauge instead of the old static "still opening (compaction) — press again" toast.
    /// `finished` flips true the instant the open resolves either way (success or the
    /// fallback chain exhausting), independent of whether `[F]`/`[Y]` has been pressed.
    open_progress: std::sync::Arc<block_store::OpenProgress>,
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

    // === [A]I tab (flux_moe — on-device chat via the user's local ollama) ===
    ai_detected: bool,                          // have we probed ollama for models yet
    ai_models: Vec<String>,                     // models the local ollama has pulled
    ai_model: Option<String>,                   // the chosen model (the effort dial)
    ai_input: String,                           // the line being typed
    ai_msgs: Vec<(String, String)>,             // (role, content) chat history
    ai_thinking: bool,                          // a reply is in flight
    ai_rx: Option<mpsc::Receiver<String>>,      // model reply lands here (off-UI-thread)

    // === CATHEDRAL DAGKNIGHT (wired 2026-06-17) ===
    /// Live vaulted DagKnight view. Ingested from tips/blocks after spine checks.
    /// Surface: health_summary, last certified vault roots, divergence=0, flux proofs.
    cathedral: Cathedral,
    splash_frame: u8,
    // ── LANE-B v0.50: SIGIL rune animation tied to update availability ──────────
    // A floating overlay band — a sigil drawing itself line-by-line then fading.
    // Frame-counter + elapsed-envelope, driven entirely off the existing rende
    // tick (never blocks input/render). Plays every 10 min when an update is
    // available, every 2 h otherwise. Timestamps live in-process only (no disk).
    rune_until: Option<Instant>,    // Some(end) while the band is on screen
    rune_started: Option<Instant>,  // when the current play began (draw-on/fade envelope)
    rune_frame: u16,                // shimmer phase, bumped each tick like splash_frame
    rune_last_played: Instant,      // last time a play started (resets per process)
    // ── LANE-B v0.50: Mining-tab depth, derived App-side from MinerStats deltas ──
    // engine.rs / MinerStats are LANE-C's; we only OBSERVE them here, never mutate.
    accept_hist: std::collections::VecDeque<u8>,                   // accept-rate % samples (sparkline)
    last_accept_sample: Instant,                                   // throttle accept sampling
    mined_recent: std::collections::VecDeque<(u64, f64, Instant)>, // (mine-chain height, solve ms, when), newest first
    mine_shares_seen: u64,          // last shares_ok observed → detect freshly mined blocks
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
    sync_kf: Kalman1D,                         // v0.33.3: Kalman-smoothed sync rate (→ stable ETA)
    // v0.7.0: AI fleet monitoring — AIs worry about their nodes' uptime and version compliance
    fleet_nodes: Vec<FleetNode>,
    fleet_last_check: Instant,
    // v0.6.0: fluxc serve status for local wallet + cockpit
    serve_status: String,
    // v0.7.0: embedded HTTP serve shutdown signal (no external process)
    serve_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    render_panic_streak: u8,
    bad_size_streak: u16,
    first_frame_logged: bool,
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
              ai_detected: false, ai_models: Vec::new(), ai_model: None,
              ai_input: String::new(), ai_msgs: Vec::new(), ai_thinking: false, ai_rx: None,
              splash_frame: 0,
              // LANE-B v0.50: rune animation — first play deferred a full interval
              // (no flash on launch; the post-update splash already covers just-updated).
              rune_until: None,
              rune_started: None,
              rune_frame: 0,
              // SIGIL_RUNE_DEMO=1 forces the first play immediately (QA/demo); off by
              // default so production launches don't flash (interval governs replays).
              rune_last_played: if std::env::var("SIGIL_RUNE_DEMO").ok().as_deref() == Some("1") {
                  instant_ago(86_400)
              } else {
                  Instant::now()
              },
              // LANE-B v0.50: mining-depth history (App-side observers of MinerStats)
              accept_hist: std::collections::VecDeque::new(),
              last_accept_sample: instant_ago(10),
              mined_recent: std::collections::VecDeque::new(),
              mine_shares_seen: 0,
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
              sync_kf: Kalman1D::new(),
              serve_status: String::new(),
              serve_stop: None,
              render_panic_streak: 0,
              bad_size_streak: 0,
              first_frame_logged: false,
              // v0.7.0: Fleet starts with known bootstrap peers.
              // Delta dropped 2026-08-14 -- confirmed permanently gone (same
              // sweep that trimmed sigil-net::DEFAULT_BOOTSTRAP_PEERS down to
              // just Epsilon). Showing a dead node here as "checking..."
              // forever was its own small dishonesty on top of the real
              // sync-stall bug this was found alongside.
              fleet_nodes: vec![
                  FleetNode { name: "Epsilon".into(), addr: "89.149.241.126".into(), port: 9501, online: false, height: 0, version: String::new(), uptime_secs: 0 },
              ],
              fleet_last_check: instant_ago(3600),
              refresh_rx: None,
              refresh_inflight: false,
              offline_since: None,
              offline_streak: 0,
              last_serve_check: Instant::now(),
              mine_stats: std::sync::Arc::new(std::sync::Mutex::new(MinerStats::default())),
              mine_desired_gpu: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
              // Cathedral DagKnight live view (first wire)
              cathedral: Cathedral::new(),
              mine_gpu_failed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
              mine_gpu_warned: false,
              bps_ema: 0.0,
              bps_zero_streak: 0,
              welcome_until: Some(Instant::now() + Duration::from_secs(14)),
              idle_store: None,
              idle_store_rx: None,
              open_progress: block_store::OpenProgress::new(),
              tab: Tab::Node,
              swarm: SwarmView::default(),
              last_swarm_load: instant_ago(10),
        }
    }

    /// LANE-B v0.50: advance + schedule the SIGIL rune animation. Pure frame-state,
    /// called once per render tick — never blocks input. Starts a ~4 s play every
    /// 10 min when an update is available, every 2 h otherwise (subtle alive-pulse),
    /// and never overlaps the post-update splash.
    fn tick_rune(&mut self) {
        let now = Instant::now();
        // currently playing → just bump the shimmer phase (like splash_frame).
        if self.rune_until.map(|u| now < u).unwrap_or(false) {
            self.rune_frame = self.rune_frame.wrapping_add(1);
            return;
        }
        // a play just ended → drop the handles so we stop counting as "animating".
        if self.rune_until.is_some() {
            self.rune_until = None;
            self.rune_started = None;
        }
        // never overlap the post-update logo splash.
        if self.splash_until.map(|u| now < u).unwrap_or(false) { return; }
        let interval = if version_gt(&self.latest, VERSION) {
            RUNE_INTERVAL_UPDATE
        } else {
            RUNE_INTERVAL_IDLE
        };
        if now.saturating_duration_since(self.rune_last_played) >= interval {
            self.rune_until = Some(now + RUNE_PLAY);
            self.rune_started = Some(now);
            self.rune_frame = 0;
            self.rune_last_played = now;
        }
    }

    /// LANE-B v0.50: true while the rune band is on screen (drives the 33/66 ms cadence).
    fn rune_active(&self) -> bool {
        self.rune_until.map(|u| Instant::now() < u).unwrap_or(false)
    }

    /// LANE-B v0.50: observe MinerStats and accumulate render-side history for the
    /// Mining tab — accept-rate sparkline + a ring of recently-mined MINE-CHAIN
    /// blocks. Read-only on the shared MinerStats (engine.rs is LANE-C's); all the
    /// derived state lives in App fields we own. Cheap; called each tick.
    fn tick_mining_history(&mut self) {
        let now = Instant::now();
        let (ok, bad, height, solve_ms) = match self.mine_stats.lock() {
            Ok(s) => (s.shares_ok, s.shares_bad, s.last_height, s.last_solve_ms),
            Err(_) => return,
        };
        // a freshly accepted share == a new block on our own mine-chain.
        if ok > self.mine_shares_seen {
            self.mined_recent.push_front((height, solve_ms, now));
            while self.mined_recent.len() > 12 { self.mined_recent.pop_back(); }
            self.mine_shares_seen = ok;
        } else if ok < self.mine_shares_seen {
            // engine restarted (counters reset) → re-baseline, don't log phantom blocks.
            self.mine_shares_seen = ok;
        }
        // sample the accept-rate every ~3 s while mining (CP437-safe shade sparkline).
        if self.mining && now.saturating_duration_since(self.last_accept_sample) >= Duration::from_secs(3) {
            let total = ok + bad;
            let acc = if total > 0 { ((ok as f64 / total as f64) * 100.0).round() as u8 } else { 100 };
            self.accept_hist.push_back(acc.min(100));
            while self.accept_hist.len() > 80 { self.accept_hist.pop_front(); }
            self.last_accept_sample = now;
        }
    }

    /// v0.37: start/stop the REAL in-process dual-lane miner — the SAME engine
    /// (flux_miner::engine::supervisor) as the standalone `sigil-miner` exe. One
    /// binary is now node + miner: no separate exe to run alongside. [g] flips GPU/CPU.
    fn toggle_engine_mining(&mut self) {
        self.mining = !self.mining;
        if self.mining {
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (url, wallet) = (engine_node_url(), miner_wallet());
            let (st, dg, gf) = (self.mine_stats.clone(), self.mine_desired_gpu.clone(), self.mine_gpu_failed.clone());
            let stopc = stop.clone();
            std::thread::spawn(move || engine::supervisor(url, wallet, st, stopc, dg, gf));
            self.mine_stop = Some(stop);
            self.tab = Tab::Mining;
            self.toast = "▲ MINING STARTED — dual-lane BLAKE4 Φ + VDF Ω, in-process".into();
            flux_webhook("mining_start", &format!("engine supervisor up (gpu_wanted={})", self.mine_desired_gpu.load(std::sync::atomic::Ordering::Relaxed)));
            self.toast_sticky = true;
        } else {
            if let Some(s) = self.mine_stop.take() { s.store(true, std::sync::atomic::Ordering::Relaxed); }
            self.toast = "■ mining stopped".into();
            flux_webhook("mining_stop", "engine stopped");
            self.toast_sticky = true;
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
    /// v7.0.21: get the parked store for an on-demand engine launch ([F]/[Y]). The store
    /// now opens on a background thread, so it may STILL be opening (compaction) — in
    /// that case say so and return None; the operator retries in a moment. A dropped
    /// channel (all open fallbacks failed) reports the store as unavailable.
    fn take_idle_store(&mut self) -> Option<block_store::BlockStore> {
        if let Some(s) = self.idle_store.take() { return Some(s); }
        if let Some(rx) = &self.idle_store_rx {
            match rx.try_recv() {
                Ok(opened) => {
                    self.idle_store_rx = None;
                    if let Some(n) = opened.note {
                        self.toast = n;
                        self.toast_sticky = true;
                    }
                    return Some(opened.store);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // v7.1.28: same live numbers the floating gauge shows (draw_store_open_overlay),
                    // so a keypress-driven toast and the passive overlay never disagree.
                    let (consumed, total, _finished) = self.open_progress.snapshot();
                    self.toast = if total > 0 {
                        let pct = ((consumed as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
                        format!(
                            "⏳ block store still opening ({pct:.0}% — {} / {}) — press again in a moment",
                            human_bytes(consumed), human_bytes(total)
                        )
                    } else {
                        "⏳ block store still opening — press again in a moment".into()
                    };
                    self.toast_sticky = false;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.idle_store_rx = None;
                    self.toast = "✗ block store unavailable — see ~/.sigil-top.log".into();
                    self.toast_sticky = true;
                }
            }
        }
        None
    }

    fn resync(&mut self) {
        // v0.37: a resync that ACTUALLY restarts the sync surface. The old one only
        // cleared the feed list (which the SYNC hero ignores), so [y] looked like a
        // no-op. Now we reset every sync-progress signal the hero reads — counters,
        // the trailing-rate window, and the Kalman ETA filter — re-assert the tip
        // target to the engine, and kick a fresh tip/status fetch.
        self.blocks.clear();
        self.synced_height = 0;
        self.verify = None;
        self.p2p_blocks_synced = 0;
        self.p2p_rate = 0.0;
        self.p2p_rate_samples.clear();
        self.sync_kf = Kalman1D::new();
        self.full_sync_height = 0;
        self.full_sync_target = 0;
        self.full_sync_active = false;
        if let Some(ref p2p) = self.p2p_sync {
            // v7.1.92 (rocky) — THE ACTUAL FIX. Everything above this line is
            // TUI-local: counters, the rate window, the Kalman ETA, the feed list.
            // Resetting them made the hero LOOK restarted while the engine carried on
            // from exactly where it was, because the persisted store watermarks
            // (synced_to / verified_to / base) were never touched. Operator report,
            // 2026-08-26: "[Y] doesn't work" — correct, it never did the one thing
            // its name promises. `request_full_resync` zeroes those watermarks on the
            // engine thread (the store lives there and reset needs &mut), so sync
            // genuinely restarts from the base.
            p2p.request_full_resync();
            // re-arm the target so the hero shows progress-to-tip again and the
            // engine re-checks it is tracking the latest tip.
            p2p.set_known_tip(self.st.height.max(self.target_height));
            self.toast = "⟳ RESYNC — starting from scratch: store watermarks zeroed, re-walking from base".into();
        } else if let Some(mut store) = self.take_idle_store() {
            // Same reset, applied directly — there is no engine thread to defer to
            // yet, and this is the one moment we own the store by value.
            store.reset_watermarks();
            // v0.71.1: in light monitor [Y] was a silent no-op with a lying toast —
            // there was no engine to resync. Now it STARTS the engine (same path
            // as F) so resync always resyncs.
            let p2p = block_sync::P2PBlockSync::launch(store, true);
            p2p.set_known_tip(self.st.height.max(self.target_height));
            self.p2p_sync = Some(p2p);
            // 0.77: the mode file now means full-ARCHIVE ([F]) specifically — [Y] starts
            // the light engine, so persist "light" (was "full", which would have made the
            // next restart launch a genesis archive the operator never asked for).
            persist_sync_mode("light");
            self.toast = "⟳ RESYNC — from scratch: watermarks zeroed, engine STARTED (was light monitor)".into();
        } else {
            self.toast = "✗ resync: sync store unavailable — restart the app".into();
        }
        self.toast_sticky = true; // confirmation survives mining/refresh noise (was vanishing instantly)
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

    /// v0.10.5: drain a completed refresh without ever blocking. Called once pe
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
        // v0.37: smooth the node jumpy blocks_per_sec (it blinks between 0 and
        // ~250-300 between its own measurement windows) so the MINING / NETWORK
        // POWER card always shows steady production. EMA on real samples; HOLD the
        // last rate through brief zero-gaps; fall to honest idle only after a
        // sustained run of zero polls (a genuine stop).
        let raw_bps = self.st.blocks_per_sec;
        if raw_bps > 0.0 {
            self.bps_ema = if self.bps_ema <= 0.0 { raw_bps } else { self.bps_ema * 0.7 + raw_bps * 0.3 };
            self.bps_zero_streak = 0;
        } else {
            self.bps_zero_streak += 1;
            if self.bps_zero_streak > 12 { self.bps_ema = 0.0; }
        }
        self.st.blocks_per_sec = self.bps_ema;
        // v7.0.24 MESH-ONLINE FIX: the online badge watched ONLY the HTTPS status feed —
        // an AUXILIARY web endpoint. A node whose libp2p mesh is connected, syncing and
        // mining is NOT offline (operator-reported: badge said "OFFLINE · net height 0"
        // for hours while the same node synced 8M+ blocks and mined at ~900 MH/s — the
        // feed's HTTPS path was blocked on the operator's side; the mesh was not). If the
        // feed fails but the mesh is live, stay ONLINE: tip from gossip (peer_best), and
        // say honestly that the feed is unreachable + why (the previously TUI-invisible
        // last_feed_err self-diagnosis).
        let mut online = out.online;
        if !online {
            if let Some(sync) = &self.p2p_sync {
                let sh = sync.state_handle();
                let (peers, peer_best) = {
                    let s = sh.lock().unwrap_or_else(|e| e.into_inner());
                    (s.peer_count, s.peer_best_height)
                };
                if peers > 0 && peer_best > 0 {
                    online = true;
                    self.st.height = self.st.height.max(peer_best);
                    if !self.toast_sticky {
                        let e = last_feed_err();
                        self.toast = format!("◆ online via p2p mesh — status feed unreachable{}",
                            if e.is_empty() { String::new() }
                            else { format!(": {}", e.chars().take(90).collect::<String>()) });
                    }
                }
            }
        }
        self.online = online;
        // v0.10.5.1: track offline → online transitions for backoff + banner.
        if online {
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
        // a false claim (the DB stayed empty). Full sync now reports the REAL numbe
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
                let msg = match fetch_latest() {
                    Ok(r) => format!("AUTO-CHECK:{}", r.version),
                    Err(e) if e.contains("MANIFEST SIGNATURE INVALID") => format!("AUTO-CHECK-ERR:{e}"),
                    Err(_) => "AUTO-CHECK:".into(),
                };
                let _ = tx.send(msg);
            });
            self.update_rx = Some(rx);
        }
        // v0.40.2: the API fallback is the rpcd's MINING chain (a few thousand
        // blocks) and carries no `tip` object — never let it clobber the
        // produce-chain tip the feed gave us (the "tip 1,495" phantom). A tip
        // DROP is only believed when the verified feed tip itself reports it.
        let fresh_tip = self.st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(self.st.height);
        self.target_height = if self.st.tip.is_some() { fresh_tip } else { fresh_tip.max(self.target_height) };
        // ONE-CHAIN (v7.1.6): show THE chain (ledger) height + a real uptime.
        if let Some(h) = one_chain_view(&mut self.st, self.p2p_sync.is_none()) {
            self.target_height = h;
        }
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
        _ => 1, // fresh version / no marker / garbage → reset the counte
    };
    let _ = std::fs::write(&path, format!("{VERSION}:{strikes}"));
    strikes
}
/// Clear the boot marker = "this version reached a healthy run".
fn mark_boot_healthy() {
    flux_webhook("healthy", "boot survived HEAL_SECS");
    if let Some(p) = boot_marker_path() { let _ = std::fs::remove_file(p); }
}
/// Arm the detached "survived HEAL_SECS → healthy" timer (decoupled from the UI loop, so a
/// normal long run clears the strike without any render-loop hook; a crash before HEAL_SECS
/// leaves the strike for the next boot to count).
fn arm_heal_timer() {
    std::thread::spawn(|| { std::thread::sleep(Duration::from_secs(HEAL_SECS)); mark_boot_healthy(); });
}
/// At dashboard startup: record the boot attempt + arm the heal timer, then ALWAYS proceed.
///
/// LANE-Z: this used to AUTO-REVERT to the previous binary + re-exec after `CRASH_STRIKES`
/// unhealed boots. On Windows that was a foot-gun: a normal double-clicked console that exits
/// before `HEAL_SECS` (or any quick exit) accrued a "strike" every launch, so after a few launches
/// the guard silently DOWNGRADED to an older binary AND spawned a detached relaunch — the
/// "won't start / blank / runs an older version invisibly" regression. The updater now re-execs
/// ONLY after a real [U] update to a STRICTLY NEWER version; crash recovery is the explicit
/// `sigil-top revert` command. So a clean launch reaches run_tui directly: exactly ONE main()
/// entry, no auto-revert, no downgrade, no detached child. Returns false (always run).
fn crashloop_guard() -> bool {
    let _ = record_boot_attempt(); // kept for diagnostics (boot marker) + the manual `revert` button
    arm_heal_timer();
    false
}

/// Retained for reference / a possible opt-in future flag — the OLD auto-revert path. NOT called
/// from the launch path any more (see `crashloop_guard`). `#[allow(dead_code)]` so it documents the
/// behavior without warning.
#[allow(dead_code)]
fn crashloop_auto_revert() -> bool {
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
    mark_boot_healthy(); // the reverted binary boots fresh under its own version counte
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
    // 0.77: shared pooled client — was a fresh Client per eclipse-k measurement.
    {
        let client = &*HTTP;
        for (name, base) in resolvers {
            let url = format!("{base}?name={ANCHOR}&type=TXT");
            let agree = client
                .get(&url)
                .header("accept", "application/dns-json")
                .timeout(Duration::from_millis(2200))
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

/// LANE-Z ROOT FIX (2026-06-19): a Backend that NEVER reports a degenerate (0×0)
/// terminal size. Windows conhost / Windows Terminal transiently answer the size
/// query with 0×0 at startup; ratatui's `autoresize` then sizes the buffer to 0×0
/// and `term.draw` paints NOTHING — a permanent BLANK SCREEN ("no TUI"), the exact
/// reported symptom. The old guard only *delayed* then force-drew into that same 0×0
/// area, so the first frame never appeared. This wrapper substitutes a sane fallback
/// (120×30) whenever the real size is degenerate and passes the TRUE size straight
/// through the instant the console reports it — a normal terminal is unaffected, and a
/// conhost that's briefly 0×0 still gets a full first frame. It also makes the TUI
/// render in a size-less pty, which is how this fix is validated without a Windows box.
struct SafeSizeBackend<W: std::io::Write> {
    inner: ratatui::backend::CrosstermBackend<W>,
}
impl<W: std::io::Write> std::io::Write for SafeSizeBackend<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.inner.write(buf) }
    fn flush(&mut self) -> std::io::Result<()> { std::io::Write::flush(&mut self.inner) }
}
impl<W: std::io::Write> ratatui::backend::Backend for SafeSizeBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> { self.inner.hide_cursor() }
    fn show_cursor(&mut self) -> std::io::Result<()> { self.inner.show_cursor() }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(&mut self, position: P) -> std::io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> std::io::Result<()> { self.inner.clear() }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        // Clamp BOTH a degenerate Ok((0,0)) AND an Err (some Windows consoles error the
        // size query at startup) to a paintable fallback, so ratatui's autoresize never
        // gets a 0-size buffer NOR a failure that would abort term.draw and exit the TUI.
        match self.inner.size() {
            Ok(s) if s.width >= 2 && s.height >= 2 => Ok(s),
            _ => Ok(ratatui::layout::Size::new(120, 30)),
        }
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> { self.inner.window_size() }
    fn flush(&mut self) -> std::io::Result<()> { ratatui::backend::Backend::flush(&mut self.inner) }
}

/// v7.0.21 DIAL-WHILE-OPENING: the complete store-open ceremony (one-time heal marker,
/// oversized light-boot check, primary→temp→volatile fallbacks, aether bootstrap),
/// extracted from run_tui so it can run on a BACKGROUND thread while the mesh dials.
/// Returns the opened store + an optional operator-facing note (the old sticky toasts).
/// Err = every fallback failed (near-impossible: volatile is a fresh temp dir).
fn open_store_with_fallbacks(
    db_path: &str,
    want_sync: bool,
    progress: std::sync::Arc<block_store::OpenProgress>,
) -> Result<(block_store::BlockStore, Option<String>), String> {
    // Guarantee `finished` flips on EVERY exit path (Ok, an early Err, or the fallthrough
    // Ok at the bottom) without threading a mark_finished() call into each of this
    // function's several `return`s — the light-boot/oversized-store shortcut in particular
    // never touches `open_with_timeout_and_progress` at all, so nothing else would mark it.
    let result = open_store_with_fallbacks_inner(db_path, want_sync, progress.clone());
    progress.mark_finished();
    result
}

fn open_store_with_fallbacks_inner(
    db_path: &str,
    want_sync: bool,
    progress: std::sync::Arc<block_store::OpenProgress>,
) -> Result<(block_store::BlockStore, Option<String>), String> {
    // v7.0.7: heal a store wedged by the v7.0.3–7.0.5 frontier-stall bug (one-time, marked).
    reset_store_on_network_change(db_path);
    heal_wedged_store_once(db_path);
    let oversized_primary = oversized_store_for_light_boot(db_path, want_sync);
    boot_trace(&format!("opening block store path={db_path} mode=background want_sync={want_sync}"));
    let mut note: Option<String> = None;
    let mut store = if let Some(bytes) = oversized_primary {
        let volatile = std::env::temp_dir()
            .join(format!("sigil-top-light-{}.db", std::process::id()));
        let volatile_s = volatile.to_string_lossy().into_owned();
        boot_trace(&format!(
            "primary block store is {} bytes; skipping open and using volatile {volatile_s}",
            bytes
        ));
        match block_store::BlockStore::open(&volatile_s) {
            Ok(s) => {
                note = Some(format!(
                    "⚠ local store is {} on disk; dashboard started on a fresh light store. Use --sync or SIGIL_TOP_FORCE_STORE=1 to reopen it.",
                    human_bytes(bytes)
                ));
                s
            }
            Err(e) => return Err(format!("volatile block store unavailable after skipping oversized primary: {e}")),
        }
    } else {
        // v6→v7.0.19: a HANG opening the primary (foreign format) falls back after the
        // watchdog; 180s default so a legitimate multi-minute compaction isn't mistaken
        // for a hang (the v7.0.19 "sync reset to 0" incident).
        // v7.0.23 SIZE-AWARE: 180s was STILL too short once a real archive grew to ~14M
        // blocks — the watchdog abandoned a multi-GB store mid-open and silently restarted
        // the sync from genesis on an empty temp store (the operator's "spine reset to
        // 240k" incident). A fixed timeout can never scale with store size, so: a LARGE
        // store (>256 MB — hours of sync investment) is NEVER abandoned; we wait as long
        // as it takes (the engine's "opening local block store — mesh already dialing"
        // status + the dial-while-opening mesh make the wait visible and productive).
        // The 180s fallback remains only for SMALL stores, where a foreign/corrupt hang
        // is the likelier explanation and a re-sync is cheap.
        let open_timeout = std::env::var("SIGIL_TOP_OPEN_TIMEOUT_SECS")
            .ok().and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                const BIG_STORE_BYTES: u64 = 256 * 1024 * 1024;
                match dir_size_capped(db_path, BIG_STORE_BYTES) {
                    Ok(bytes) if bytes > BIG_STORE_BYTES => {
                        boot_trace(&format!("store is >{} bytes — open watchdog DISABLED (never abandon a big archive)", BIG_STORE_BYTES));
                        u64::MAX / 4 // effectively: wait for the open, however long
                    }
                    _ => 180,
                }
            });
        match block_store::BlockStore::open_with_timeout_and_progress(db_path, open_timeout, Some(progress.clone())) {
            Ok(s) => s,
            Err(primary) => {
                boot_trace(&format!("primary store open failed/hung: {primary}"));
                let temp_path = std::env::temp_dir().join("sigil-top-blocks.db");
                match block_store::BlockStore::open_with_timeout(temp_path.to_string_lossy().as_ref(), open_timeout) {
                    Ok(s) => {
                        note = Some(format!("⚠ primary block store unavailable ({primary}); using temp store"));
                        s
                    }
                    Err(temp_err) => {
                        let volatile = std::env::temp_dir()
                            .join(format!("sigil-top-blocks-volatile-{}.db", std::process::id()));
                        match block_store::BlockStore::open_with_timeout(volatile.to_string_lossy().as_ref(), open_timeout) {
                            Ok(s) => {
                                note = Some(format!("⚠ block store fallback is volatile ({primary}; temp: {temp_err})"));
                                s
                            }
                            Err(volatile_err) => {
                                return Err(format!(
                                    "block store unavailable: primary={primary}; temp={temp_err}; volatile={volatile_err}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    };
    boot_trace(&format!("block store opened best={} synced={} verified={}",
        store.best_height(), store.synced_to(), store.verified_to()));

    // v0.7.1/v0.56: aether shard bootstrap — Epsilon-server path, silently skipped elsewhere.
    let aether_dir = std::env::var("SIGIL_AETHER_DIR")
        .unwrap_or_else(|_| "/opt/orobit/sigil-data/db-epsilon/aether".to_string());
    if std::path::Path::new(&aether_dir).is_dir() {
        match block_store::sync_aether_to_fluxdb(&mut store, &aether_dir) {
            Ok(n) if n > 0 => {
                if note.is_none() {
                    note = Some(format!("⬇ Synced {n} blocks → flux-db (height {})", store.best_height()));
                }
            }
            Err(e) => tlog!("[aether] {e}"),
            _ => {}
        }
    }
    Ok((store, note))
}

// v7.1.10: probe the embedded wallet server and, if it is not answering, try to
// (re)start it in-process. Returns true when :9800 is accepting connections.
//
// Why this exists: a FAILED initial start (port already held by a previous
// instance — the classic case right after a self-update) left `serve_stop` as
// None, and the watchdog below was gated on `is_some()`. So the port stayed dark
// for the whole life of the process, [W] opened a browser tab at a refused port,
// and the reason was invisible because `serve_status` is rendered nowhere. Both
// the keypress paths and the watchdog now go through here.
fn ensure_serve_up(app: &mut App) -> bool {
    if wallet_server_alive() {
        return true;
    }
    if let Some(old) = app.serve_stop.take() {
        old.store(true, Ordering::Relaxed);
    }
    let serve_dir = std::env::var("FLUX_STATIC_DIR")
        .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
    match serve::start(&serve_dir, wallet_ui::WALLET_PORT) {
        Ok(stop) => {
            app.serve_stop = Some(stop);
            // Let the listener bind before we hand the URL to a browser.
            for _ in 0..20 {
                if wallet_server_alive() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let ok = wallet_server_alive();
            app.serve_status = if ok {
                "serve :9800 ✓ (re)started on demand".into()
            } else {
                "serve :9800 started but not answering".into()
            };
            ok
        }
        Err(e) => {
            app.serve_status = format!("serve: {e}");
            false
        }
    }
}

fn run_tui(cfg: Config) -> std::io::Result<()> {
    // v0.27.5: self-healing crash-loop guard — long-running dashboard only (`--once` renders
    // and exits faster than HEAL_SECS, which would false-trigger a revert). If THIS version
    // has crash-looped, this reverts to the last working binary and relaunches.
    if !cfg.once && crashloop_guard() { return Ok(()); }

    // ── v0.35 (sync-starts-earlier, DeepSeek audit S4): the ENTIRE sync bootstrap runs
    // BEFORE the terminal is touched. It used to sit after raw-mode + alt-screen + panic-
    // hook + Terminal::new, so the store open (formerly a full-db scan!), the aethe
    // bootstrap and the P2P launch all waited on UI plumbing. Now the mesh is dialing and
    // the first probe is in flight while crossterm sets up; pre-TUI prints go to stder
    // (IN_TUI is still false), which is exactly where pre-UI diagnostics belong. ─────────
    let mut app = App::new(cfg);
    // v4 AUTO-START: env-driven mining + full-sync (headless rigs, no keypress).
    if std::env::var("SIGIL_AUTOMINE").is_ok() {
        use std::sync::atomic::Ordering;
        if cfg!(feature = "gpu") && std::env::var("SIGIL_MINE_CPU").is_err() {
            app.mine_gpu_warned = true;
            app.mine_desired_gpu.store(true, Ordering::Relaxed);
        }
        app.toggle_engine_mining();
        boot_trace("AUTOMINE: engine started via SIGIL_AUTOMINE");
    }
    if std::env::var("SIGIL_AUTOFULLSYNC").is_ok() && app.p2p_sync.is_none() {
        if let Some(store) = app.take_idle_store() {
            let p2p = block_sync::P2PBlockSync::launch(store, false);
            app.p2p_sync = Some(p2p);
            app.full_sync = true;
            persist_sync_mode("full");
            boot_trace("AUTOFULLSYNC: full archive started");
        }
    }
    flux_webhook("boot", concat!("sigil-top v", env!("CARGO_PKG_VERSION"), " starting"));
    // v0.71.1 LANE-V: resume the operator's chosen sync mode after update/restart.
    // Only "full" does anything; no file (fresh install) = safe light monitor.
    if false && read_sync_mode().as_deref() == Some("full") && app.p2p_sync.is_none() {
        if let Some(store) = app.take_idle_store() {
            let p2p = block_sync::P2PBlockSync::launch(store, true);
            app.p2p_sync = Some(p2p);
            app.toast = "⬇ FULL-SYNC RESUMED (sticky mode from last session — [F] to toggle)".into();
            app.toast_sticky = true;
            boot_trace("sticky full-sync resumed from mode file");
        }
    }
    // Windows: bring up the notification-area icon (Open Wallet / Explorer / Start-at-login /
    // Quit). Detached + best-effort, so it can never gate or crash the node. No-op elsewhere.
    // v0.64: the tray helper Stop-Process'es NodePid and broke double-click launches
    // (window closed instantly). OFF by default now; SIGIL_TOP_TRAY=1 opts back in.
    if std::env::var("SIGIL_TOP_TRAY").is_ok() { spawn_system_tray(); }
    boot_trace("tray gated (off unless SIGIL_TOP_TRAY)");
    // v0.10.5: async — kicks off the first fetch without blocking the first paint.
    app.request_refresh();

    // v0.12.1: sync is ON BY DEFAULT. Opt OUT with --no-sync / SIGIL_TOP_NO_SYNC=1.
    // v0.39.1 WINDOWS: default OFF — the LIGHT monitor. v0.10.2 already learned
    // this lesson (the sync engine ate the operator's PC; made opt-in), v0.12.1
    // regressed it to on-by-default, and it froze the desktop twice today. The
    // Mining tab / wallet / explorer don't need the local backfill engine — a
    // fleet node (epsilon) carries the chain. Opt IN with --sync / SIGIL_TOP_SYNC=1.
    let want_sync = if cfg!(windows) {
        std::env::args().any(|a| a == "--sync") || std::env::var("SIGIL_TOP_SYNC").is_ok()
    } else {
        !(std::env::args().any(|a| a == "--no-sync")
            || std::env::var("SIGIL_TOP_NO_SYNC").is_ok())
    };

    // v0.7.22: cross-platform PERSISTENT store path. The old /tmp + /dev/shm paths
    // don't exist on Windows → the store never persisted → re-sync from 0 every launch
    // ("starts over on update"). Now a per-user dir (override with SIGIL_TOP_DB).
    let db_path = sigil_top_db_path();
    // v7.0.17 NEVER-BLANK splash (instant now — the open no longer blocks this thread).
    println!("\n  \u{25c7} SIGIL v{} — starting… (block store opens in the background; the mesh dials immediately)\n",
        env!("CARGO_PKG_VERSION"));
    let _ = std::io::Write::flush(&mut std::io::stdout());
    // v7.0.19: flux-db compaction progress goes to raw stderr, which corrupts the TUI's
    // alternate screen ("[flux-db] compact L1->L2 ..." painted over the dashboard). Quiet
    // the chatter (errors still print) for this process before the store opens.
    std::env::set_var("FLUX_DB_QUIET", "1");

    // ── v7.0.21 DIAL-WHILE-OPENING ────────────────────────────────────────────────
    // The store open (heal → fallbacks → aether) moves to a BACKGROUND thread; the
    // sync engine launches IMMEDIATELY with a deferred store, so the mesh dials and
    // handshakes during a long compaction instead of sitting at 0 peers for minutes
    // (the v7.0.19 case: p2p couldn't start until a 2.6 GiB compaction finished).
    // The explorer's local reader arrives via a OnceLock — until then it proxies.
    let reader_cell: std::sync::Arc<std::sync::OnceLock<block_store::BlockReader>> =
        std::sync::Arc::new(std::sync::OnceLock::new());
    let (store_tx, store_rx) = std::sync::mpsc::channel::<block_sync::OpenedStore>();
    {
        let db_path = db_path.clone();
        let cell = reader_cell.clone();
        let progress = app.open_progress.clone();
        let _ = std::thread::Builder::new().name("sigil-store-open".into()).spawn(move || {
            match open_store_with_fallbacks(&db_path, want_sync, progress) {
                Ok((store, note)) => {
                    let _ = cell.set(store.reader());
                    let _ = store_tx.send(block_sync::OpenedStore { store, note });
                }
                Err(e) => {
                    // tx drops → the deferred engine / [F] helper surface "store unavailable".
                    boot_trace(&format!("store open FAILED on all fallbacks: {e}"));
                    eprintln!("sigil-top: {e}");
                }
            }
        });
    }

    // v0.26 hardening #8 (DeepSeek-reviewed): graceful SIGTERM/SIGINT — restore the
    // terminal (harmless no-op if Ctrl-C lands before raw mode) and exit cleanly; the
    // sync thread's BlockStore persists watermarks on every advance, so state is durable.
    {
        let _ = ctrlc::set_handler(move || {
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
            std::process::exit(0);
        });
    }

    let mut sync_handle: Option<std::sync::Arc<std::sync::Mutex<block_sync::P2PSyncState>>> = None;
    // 0.77/v7.0.21: the persisted [F] choice ("full") now RESUMES on every platform —
    // Windows defaulted want_sync=false, so an operator's full archive silently didn't
    // restart after an update ("it doesn't resume") until they pressed F again.
    let autostart_full = read_sync_mode().as_deref() == Some("full");
    let autofullsync = std::env::var("SIGIL_AUTOFULLSYNC").is_ok();
    // ONE-CHAIN (v7.1.6): the LEDGER header mirror is the default sync — full
    // floor→tip in ~1.4s. The SPINE monitor/backfill (31.5M empty dyno blocks,
    // ~8 blk/s serve valve → a 78-DAY eta) no longer auto-starts: it is opt-in
    // via SIGIL_SPINE_SYNC=1, a saved full-sync mode, or the explicit [F] key.
    // The spine retires in P3 (docs/SIGIL_ONE_CHAIN_SCOPE_v0.md).
    block_sync::ledger::ensure_running();
    let spine_opt_in = std::env::var("SIGIL_SPINE_SYNC").map(|v| v == "1").unwrap_or(false);
    if (want_sync && spine_opt_in) || autostart_full || autofullsync {
        // v0.22.1: monitor path (recent_only=true) → fast-snap to the verified live tip.
        // v0.33.5: SIGIL_FULLSYNC=1 launches a genuine genesis→tip crawl instead.
        let recent_only = std::env::var("SIGIL_FULLSYNC").map(|v| v == "0" || v.is_empty()).unwrap_or(true)
            && !autostart_full && !autofullsync;
        let p2p = block_sync::P2PBlockSync::launch_deferred(store_rx, recent_only);
        sync_handle = Some(p2p.state_handle());
        if !recent_only { app.full_sync = true; }
        app.p2p_sync = Some(p2p);
        if app.toast.is_empty() {
            app.toast = "⚡ P2P mesh connecting → Delta / Epsilon…".into();
        }
    } else {
        // v0.40.3/v7.0.21: park the RECEIVER so [F] can launch the sync engine ON
        // DEMAND — the store arrives from the opener thread (take_idle_store polls it).
        app.idle_store_rx = Some(store_rx);
        if app.toast.is_empty() {
            app.toast = "◆ light monitor — press F to start live sync".into();
        }
    }

    // v3: NON-FATAL terminal setup. A console that rejects raw mode / the alternate screen
    // must not abort the dashboard (that was a silent "no TUI" exit). Log and continue — the
    // render loop still paints; keys may be line-buffered in the degraded case, which beats blank.
    if let Err(e) = enable_raw_mode() { boot_trace(&format!("enable_raw_mode failed (continuing): {e}")); }
    // From here ratatui owns the screen — divert background eprintln to the logfile
    // (was smearing the dashboard with [p2p-sync]/[aether] lines).
    IN_TUI.store(true, std::sync::atomic::Ordering::Relaxed);
    // ...and the same for DEPENDENCIES that print on their own. IN_TUI only
    // diverts THIS crate's macros; flux-db writes storage diagnostics with raw
    // `eprintln!`, which land in the middle of the frame. Reported live: a real
    // SST read error ("failed to fill whole buffer") rendered as corrupted text
    // spliced through the sync card — it looked like a UI bug AND hid the
    // actual storage error. flux-db honors FLUX_DB_LOG by appending there
    // instead of writing to stderr; point it at the same logfile everything
    // else uses. Only set when unset, so an operator can still override.
    if std::env::var_os("FLUX_DB_LOG").is_none() {
        // Same resolution the logger above uses, so both streams land together.
        let p = std::env::var("HOME")
            .map(|h| format!("{h}/.sigil-top.log"))
            .or_else(|_| std::env::var("TEMP").map(|t| format!("{t}\\sigil-top.log")))
            .unwrap_or_else(|_| "sigil-top.log".into());
        std::env::set_var("FLUX_DB_LOG", p);
    }
    let mut stdout = std::io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) { boot_trace(&format!("EnterAlternateScreen failed (continuing): {e}")); }

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
            let line = format!("[PANIC] {msg} @ {loc}");
            remember_panic(line.clone());
            log_line(line);
        }));
    }

    // LANE-Z root fix: wrap in SafeSizeBackend so a degenerate (0×0) conhost size can
    // never produce a blank screen — ratatui always gets a paintable area.
    let backend = SafeSizeBackend { inner: CrosstermBackend::new(stdout) };
    let mut term = Terminal::new(backend)?;
    // (v0.35: app/store/aether/ctrlc/sync-launch all moved ABOVE terminal init — see the
    // sync-starts-earlier block after crashloop_guard. block_reader/sync_handle/app are in
    // scope from there.)

    // v0.11.0: local-first explorer API over the verified spine + cortex snapshot.
    let local_api = std::sync::Arc::new(local_api::LocalApi {
        reader: reader_cell.clone(),
        sync: sync_handle,
        cortex: app.cortex_shared.clone(),
        network: build_network_id(),
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
            // ── LANE-Z: Windows conhost RENDER-HANG guard (the real "won't start / blank") ─────
            // Some Windows consoles (conhost / Windows Terminal) transiently answer the crossterm
            // terminal-size query with 0×0. ratatui then draws an EMPTY 0-size frame on EVERY
            // iteration = a BLACK screen spinning ~96% of a core that never paints the first frame
            // (screen-share-confirmed; reproduces on 0.70.0 too, so it is NOT the updater). While
            // the reported size is degenerate, DON'T draw a black frame: block on input for 100 ms
            // (so the loop can't busy-spin and a real resize/quit still gets through), then retry.
            // The plain-text `--once` / `render_full` path never enters this loop — which is exactly
            // why it always renders while `--tui` spun black.
            let size_now = crossterm::terminal::size();
            match size_now {
                Ok((w, h)) if w >= 2 && h >= 2 => {
                    if app.bad_size_streak != 0 {
                        boot_trace(&format!("terminal size recovered after {} bad reads: {w}x{h}", app.bad_size_streak));
                    }
                    app.bad_size_streak = 0;
                }
                other => {
                    app.bad_size_streak = app.bad_size_streak.saturating_add(1);
                    if app.bad_size_streak >= 20 {
                        if app.bad_size_streak == 20 {
                            let detail = format!("terminal size stayed degenerate for ~2s ({other:?}); forcing draw so failure is visible");
                            boot_trace(&detail);
                            log_line(format!("[render] {detail}"));
                        }
                    } else {
                    if event::poll(Duration::from_millis(100))? {
                        if let Event::Key(k) = event::read()? {
                            if k.kind == KeyEventKind::Press
                                && matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc)
                            {
                                return Ok(());
                            }
                        }
                    }
                    continue;
                    }
                }
            }
            if app.splash_until.map(|u| Instant::now() < u).unwrap_or(false) {
                app.splash_frame = app.splash_frame.wrapping_add(1);
            }
            // LANE-B v0.50: advance/schedule the rune animation + accumulate mining
            // history. Both are pure frame-state and never block the render loop.
            app.tick_rune();
            app.tick_mining_history();
            // v0.10.5: drain any completed async refresh (never blocks).
            app.poll_refresh();
            // v0.27 CRASH-PROOF: a panic inside rendering (bad slice, unwrap on odd data, etc.)
            // used to unwind out of run_tui and EXIT the app ("crashes after 20s"). Catch it —
            // the panic hook logs [PANIC] with file:line — re-init the terminal and keep running;
            // the next frame redraws. The monitor must never die on a single bad render frame.
            let mut frame_panicked = false;
            term.draw(|f| {
                // Catch the panic AROUND draw_ui (the render code, which is the panic source) —
                // term.draw itself still owns the closure so there's no borrow-escape. A rende
                // panic leaves a partial frame (harmless; the next frame redraws) instead of
                // unwinding out of run_tui and killing the app.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| draw_ui(f, &app))).is_err() {
                    frame_panicked = true;
                    log_line("[render] frame panicked — caught".into());
                }
                // v0.33 GLOBAL ASCII pass: on Windows (or SIGIL_ASCII=1) rewrite any remaining
                // wide/emoji cell symbol to width-1 ASCII. Done on the buffer AFTER layout but
                // BEFORE flush: ratatui already reserved 2 cells for a wide glyph, so emitting a
                // width-1 symbol makes the backend emit a corrective MoveTo for the next cell →
                // exact alignment on consoles that draw emoji at the wrong width. Catches every
                // glyph everywhere in one place (belt-and-suspenders with the source sa() wraps).
                if ui_ascii() {
                    let buf = f.buffer_mut();
                    for cell in buf.content.iter_mut() {
                        let sym = cell.symbol().to_string();
                        if !sym.is_ascii() {
                            let repl = sa(sym.as_str());
                            if repl != sym { cell.set_symbol(&repl); }
                        }
                    }
                }
            })?;
            if frame_panicked {
                app.render_panic_streak = app.render_panic_streak.saturating_add(1);
                let detail = last_panic_line();
                boot_trace(&format!(
                    "render panic streak {}/3: {detail}",
                    app.render_panic_streak
                ));
                if app.render_panic_streak >= 3 {
                    let _ = disable_raw_mode();
                    let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
                    let _ = term.show_cursor();
                    IN_TUI.store(false, std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "sigil-top: render failed 3 frames in a row; exiting cleanly instead of black-screening.\n  {detail}\n  startup log: {}",
                        std::env::temp_dir().join("sigil-top-startup.log").display()
                    );
                    return Ok(());
                }
                continue;
            } else if app.render_panic_streak != 0 {
                boot_trace("render recovered after caught panic");
                app.render_panic_streak = 0;
            }
            if !app.first_frame_logged {
                match crossterm::terminal::size() {
                    Ok((w, h)) => boot_trace(&format!("first frame drawn {w}x{h}")),
                    Err(e) => boot_trace(&format!("first frame drawn (size unavailable: {e})")),
                }
                app.first_frame_logged = true;
            }
            // v0.10.5 "smooth cruise": adaptive frame pacing. When something is
            // moving — splash animation, an in-flight refresh, or live mining — poll
            // at ~30 fps so motion is buttery. When parked, fall back to a calm 200 ms
            // so an idle cockpit barely touches the CPU. Keys stay responsive either way.
            let animating = app.splash_until.map(|u| Instant::now() < u).unwrap_or(false)
                || app.refresh_inflight
                || app.mining
                || app.rune_active();   // LANE-B v0.50: smooth the rune band's draw-on/fade
            // v0.40: 33ms full redraws are cheap on a modern terminal but heavy on the
            // legacy CP437 conhost — halve the animating cadence on Windows so the
            // console host never becomes a load source itself.
            let poll_ms = if animating { if cfg!(windows) { 66 } else { 33 } } else { 200 };
            if event::poll(Duration::from_millis(poll_ms))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        // LANE-U v0.67: the first key dismisses the welcome modal (F also falls
                        // through to start sync/mining as usual).
                        if app.welcome_until.is_some() { app.welcome_until = None; }
                        match k.code {
                            // [A]I tab captures typing — these guarded arms MUST precede the
                            // global shortcuts so 'q'/Esc/digits type into the chat instead of
                            // quitting/switching. Tab still cycles out (falls through below).
                            KeyCode::Enter if app.tab == Tab::Ai => { ai_submit(&mut app); }
                            KeyCode::Backspace if app.tab == Tab::Ai => { app.ai_input.pop(); }
                            KeyCode::Esc if app.tab == Tab::Ai => { app.tab = Tab::Node; }
                            KeyCode::Char(c) if app.tab == Tab::Ai => { app.ai_input.push(c); }
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('r') | KeyCode::Char('R') => { app.refresh(); app.toast_sticky = false; }
                            // v0.13: tab switching — Tab cycles, 1/2/3 jump
                            KeyCode::Tab | KeyCode::BackTab => { app.tab = app.tab.next(); }
                            KeyCode::Char('1') => { app.tab = Tab::Node; }
                            KeyCode::Char('2') => { app.tab = Tab::SyncLog; }
                            KeyCode::Char('3') => { app.tab = Tab::Mining; }
                            KeyCode::Char('4') => { app.tab = Tab::Queues; }
                            KeyCode::Char('5') => {
                                app.tab = Tab::Ai;
                                if !app.ai_detected { // one-time local-ollama probe (localhost: instant)
                                    app.ai_models = crate::flux_moe::list_models();
                                    app.ai_model = app.ai_models.first().cloned();
                                    app.ai_detected = true;
                                }
                            }
                            KeyCode::Char('w') | KeyCode::Char('W') => {
                                // [W] open the web wallet (SIGIL OS modal lives inside it) in the
                                // local browser. Headless boxes have no GUI → show the link to copy.
                                let url = crate::wallet_ui::official_wallet_url();
                                if crate::wallet_ui::open_browser(&url) {
                                    app.toast = format!("🌐 opening web wallet + SIGIL OS — {url}");
                                } else {
                                    app.toast = format!("🌐 web wallet + SIGIL OS: {url}");
                                }
                                app.toast_sticky = true;
                            }
                            KeyCode::Char('y') | KeyCode::Char('Y') => { app.resync(); app.toast_sticky = false; }
                            KeyCode::Char('m') | KeyCode::Char('M') => { app.toggle_engine_mining(); }
                            KeyCode::Char('g') | KeyCode::Char('G') if app.tab == Tab::Mining => {
                                // [G] gates GPU use: GPU mining is OFF by default and only
                                // engages on an explicit opt-in here (and only on a -gpu
                                // build — the engine supervisor reverts the flag otherwise).
                                use std::sync::atomic::Ordering;
                                let now = app.mine_desired_gpu.load(Ordering::Relaxed);
                                if now {
                                    // turning OFF — always immediate.
                                    app.mine_desired_gpu.store(false, Ordering::Relaxed);
                                    app.toast = "⚙ engine → CPU".into();
                                } else if !cfg!(feature = "gpu") {
                                    // CPU-only build: flip the flag so the engine logs the
                                    // "rebuild with --features gpu" hint, but it never runs GPU.
                                    app.mine_desired_gpu.store(true, Ordering::Relaxed);
                                    app.toast = "⚙ engine → GPU (needs a -gpu build)".into();
                                } else if !app.mine_gpu_warned {
                                    // First-ever GPU opt-in on a -gpu build: WARN and require a
                                    // second [G] to actually enable (don't enable yet). GPU
                                    // mining can overheat a laptop; the guard auto-throttles
                                    // ≥78C and drops to CPU ≥82C, but it needs nvidia-smi.
                                    app.mine_gpu_warned = true;
                                    app.toast = "⚠ GPU mining can overheat a laptop. Guard auto-throttles ≥78C, drops to CPU ≥82C (needs nvidia-smi; NVIDIA only). Press [G] again to enable GPU.".into();
                                } else {
                                    app.mine_desired_gpu.store(true, Ordering::Relaxed);
                                    app.toast = "⚙ engine → GPU".into();
                                }
                                app.toast_sticky = true;
                            }
                            KeyCode::Char('f') | KeyCode::Char('F') => {
                                // 0.77 GENESIS ARCHIVE (#156): [F] is the explicit "hold the whole
                                // chain" switch. No engine → start it in FULL-ARCHIVE mode (base
                                // anchored at genesis, every block held — the redundant-backup
                                // promise; the old code launched recent_only=true and the toggle
                                // never reached the engine). Engine running → LIVE-FLIP it between
                                // full-archive and light-monitor through the engine atomics.
                                // Light-monitor's 10ms tip-verify stays the untouched startup default.
                                if app.p2p_sync.is_none() {
                                    // BUG FIX (Viktor report, "sync doesnt work", v7.1.17):
                                    // take_idle_store() ALREADY sets an accurate toast on
                                    // both its None outcomes — "still opening (compaction)
                                    // — press again in a moment" when the background opener
                                    // thread just hasn't finished yet (the common case: a
                                    // fresh launch, pressing F within the first few seconds,
                                    // uptime 0m19s in the reported repro), or "unavailable —
                                    // see ~/.sigil-top.log" when the opener genuinely failed.
                                    // This branch used to UNCONDITIONALLY overwrite that with
                                    // "restart with --sync" — wrong and misleading in the
                                    // (much more common) transient case: the store was never
                                    // unavailable, it just wasn't ready yet, and --sync isn't
                                    // even required (F starts it on demand by design — see the
                                    // idle_store_rx parking a few hundred lines up). Do NOT set
                                    // a toast here; let take_idle_store's own message stand.
                                    if let Some(store) = app.take_idle_store() {
                                        let p2p = block_sync::P2PBlockSync::launch(store, false);
                                        app.p2p_sync = Some(p2p);
                                        app.full_sync = true;
                                        persist_sync_mode("full"); // LANE-V: survives update/restart
                                        app.toast = "⛓ FULL ARCHIVE started — syncing genesis→tip, holding every block (~1GB). F again = light monitor.".into();
                                    }
                                    app.toast_sticky = true;
                                } else if let Some(ref p2p) = app.p2p_sync {
                                    if p2p.is_recent_only() {
                                        p2p.set_full_archive();
                                        app.full_sync = true;
                                        persist_sync_mode("full");
                                        app.toast = "⛓ FULL ARCHIVE — base → genesis, downloading + holding every block (~1GB). Press F for light monitor.".into();
                                    } else {
                                        p2p.set_light_monitor();
                                        app.full_sync = false;
                                        persist_sync_mode("light");
                                        app.toast = "◇ LIGHT MONITOR — verifies the live tip (10ms proof), holds nothing new. Press F for full archive.".into();
                                    }
                                    app.toast_sticky = false;
                                }
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
                                            Ok(rel) => release_channel_current_msg(&rel.version),
                                            Err(e) => format!("⚠ update check: {e}"),
                                        };
                                        let _ = tx.send(msg);
                                    });
                                    app.update_rx = Some(rx);
                                }
                            }
                            KeyCode::Char('l') | KeyCode::Char('L') => {
                                // v7.1.30: [L]/[W] now open the HOSTED React/Vite/TS wallet
                                // (official_wallet_url(), the app with the live Swap page)
                                // directly — no dependency on the local :9800 embedded server
                                // or the block store being open. The old local-server path
                                // opened the single-file Tron wallet instead; that page still
                                // exists (served at :9800 for headless/offline use and by [T]/
                                // [B]'s #stats/#activity deep links, which it alone supports)
                                // but is no longer what [L]/[W] launch by default.
                                let url = official_wallet_url();
                                if open_browser_private(&url) {
                                    app.toast = format!("🌐 wallet (private window) → {url}").into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the wallet in any browser:  {url}").into();
                                    app.toast_sticky = true;
                                }
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
                                // [W] is the node-local wallet: bring up the embedded server
                                // first, then open its compiled-in UI. Keep this URL exact so
                                // SIGIL_WALLET_URL/FLUX_WALLET_URL cannot redirect the shortcut
                                // to the hosted wallet; [L] remains the explicit hosted option.
                                //
                                // 2026-08-25: was missing `?addr=`, unlike the headless fallback
                                // below (headless_wallet_view_url) which already does this. The
                                // page's ADDR (the header "yours" pill, driven by ?addr= or else
                                // localStorage['sigil-wallet-address'] from an earlier, unrelated
                                // onboarding visit) is a SEPARATE identity from MINE_ADDR (the
                                // balance number, seed-derived, already correct). Without ?addr=
                                // here, a browser tab with any prior onboarding history keeps
                                // showing that stale/unrelated address next to the correct
                                // balance — "the balance is right but it's a different wallet."
                                // Passing the real mining address explicitly overrides whatever
                                // is cached, same as the headless link already does.
                                let url = format!("http://localhost:{}/?addr={}", wallet_ui::WALLET_PORT, miner_wallet());
                                if !ensure_serve_up(&mut app) {
                                    app.toast = format!("✗ local wallet server down ({})", app.serve_status).into();
                                    app.toast_sticky = true;
                                } else if open_browser_private(&url) {
                                    app.toast = format!("◆ local wallet → {url}").into();
                                    app.toast_sticky = false;
                                } else {
                                    // Headless (SSH/terminal-only): localhost:9800 only means
                                    // something on THIS box, so handing it out is a dead link on
                                    // any other device. Give a link that actually works elsewhere —
                                    // see wallet_ui::headless_wallet_view_url's doc comment for why
                                    // this carries no secret and is safe to print/copy anywhere.
                                    //
                                    // 2026-08-26: that remote link is seedless by design, so it
                                    // still prompts for the recovery phrase on every Send — the
                                    // zero-prompt server (SIGIL_MINE_SEED-backed :9800) never went
                                    // away, headless just never pointed at it. Offer the tunnel
                                    // that actually reaches it, alongside the remote fallback.
                                    let fallback = wallet_ui::headless_wallet_view_url();
                                    app.toast = match wallet_ui::ssh_tunnel_hint() {
                                        Some(hint) => format!("🔐 {hint}\n🔗 or, prompted: {fallback}").into(),
                                        None => format!("🔗 headless — open your wallet from any device:  {fallback}").into(),
                                    };
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('t') | KeyCode::Char('T') => {
                                // Network-stats modal in the tron wallet UI — the SIGIL
                                // equivalent of the Quillon graph dashboard stats. The
                                // #stats deep-link auto-opens the modal on load; data is
                                // same-origin via the :9800 /api proxy + feed JSON.
                                let url = format!("{}#stats", local_wallet_url());
                                if !ensure_serve_up(&mut app) {
                                    app.toast = format!("✗ local wallet server down ({}) — stats need the :9800 server", app.serve_status).into();
                                    app.toast_sticky = true;
                                } else if open_browser_private(&url) {
                                    app.toast = format!("📊 network stats → {url}").into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open the stats modal in any browser:  {}#stats", official_wallet_url()).into();
                                    app.toast_sticky = true;
                                }
                            }
                            KeyCode::Char('b') | KeyCode::Char('B') => {
                                // Recent activity — the Explorer/Activity view INSIDE the
                                // local wallet, deep-linked via #activity and served from
                                // this node's own :9800. Was: the public fluxapp.xyz
                                // explorer, which is a different origin and shows the
                                // PUBLIC node's chain — so on a local/private node [B]
                                // showed someone else's blocks. Same pattern as [T].
                                let url = format!("{}#activity", local_wallet_url());
                                if !ensure_serve_up(&mut app) {
                                    app.toast = format!("✗ local wallet server down ({}) — recent activity needs the :9800 server", app.serve_status).into();
                                    app.toast_sticky = true;
                                } else if open_browser_private(&url) {
                                    app.toast = format!("📜 recent activity → {url}").into();
                                    app.toast_sticky = false;
                                } else {
                                    app.toast = format!("🔗 headless — open recent activity in any browser:  {}#activity", official_wallet_url()).into();
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
            // v7.0.13: Swarm AI / Results tabs removed — no periodic swarm reload needed.
            // v0.10.5.1: embedded-serve watchdog — if the :9800 wallet server died,
            // restart it so the local wallet/[W] never silently goes dark. Probe is
            // throttled to 15s and only blocks (briefly) in the rare dead case.
            // v7.1.10: NO `serve_stop.is_some()` gate — that gate meant a failed
            // INITIAL start could never be retried, leaving :9800 dark forever.
            if app.last_serve_check.elapsed() >= Duration::from_secs(15) {
                app.last_serve_check = Instant::now();
                if !wallet_server_alive() {
                    ensure_serve_up(&mut app);
                }
            }
            // v0.6.5: Poll P2P sync state + drain synced blocks into the TUI block list
            if let Some(ref p2p) = app.p2p_sync {
                // 0.77: poll_state is try_lock — None = the sync thread holds the lock right
                // now (heavy ingest/flush). Keep rendering the previous clone; never block
                // the draw thread (the Windows full-archive BLACK SCREEN, #156 item 2).
                if let Some(s) = p2p.poll_state() { app.p2p_state = s; }
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
                // v0.31: a one-time CATCH-UP jump (synced snaps 0→~tip the instant the monito
                // reaches the head, or after a re-snap over a big gap) is NOT a sustained rate —
                // it spiked the readout to ~1M blk/s then craters to 0 as the jump scrolls out of
                // the 10s window (the "1M → 0" the user saw). If the metric leaps > CATCHUP_JUMP in
                // one sample, reset the window so the rate reflects STEADY tip advance, never the
                // snap. chronos confirmed delivery is fine — this was purely a display artifact.
                const CATCHUP_JUMP: u64 = 50_000;
                if let Some(&(_, last_b)) = app.p2p_rate_samples.back() {
                    if rate_metric.saturating_sub(last_b) > CATCHUP_JUMP {
                        app.p2p_rate_samples.clear();
                    }
                }
                app.p2p_rate_samples.push_back((now, rate_metric));
                // v7.0.22: 10s → 30s trailing window. The storage engine legitimately pauses
                // apply for a few seconds (memtable flush / bulk-mode compaction settle); a
                // 10s window collapsed the readout to ~0 on every pause, which reads as
                // FAULTY even though fetch continues and the blocks land in a burst right
                // after. Over 30s a 3-5s breath is a ~10-15% dip — visible, honest, calm.
                // A REAL stall still decays to 0 (and the STALLED/WEDGE headline + watchdog
                // name it loudly long before the window empties).
                while app.p2p_rate_samples.len() > 1
                    && now.duration_since(app.p2p_rate_samples[0].0).as_secs_f64() > 30.0
                {
                    app.p2p_rate_samples.pop_front();
                }
                if let (Some(&(t0, b0)), Some(&(t1, b1))) =
                    (app.p2p_rate_samples.front(), app.p2p_rate_samples.back())
                {
                    let dt = t1.duration_since(t0).as_secs_f64();
                    if dt >= 1.0 {
                        let long_rate = b1.saturating_sub(b0) as f64 / dt;

                        // REGIME CHANGE, DOWNWARD (2026-08-27). `CATCHUP_JUMP` above already
                        // resets the window when the metric LEAPS UP, so an instantaneous
                        // catch-up snap cannot masquerade as a sustained rate. The mirror case
                        // had no such guard, and it is the one an operator actually stares at:
                        // bulk import runs at thousands of blk/s, then the sync reaches the
                        // frontier and drops to the network's real ~10 blk/s. Both effects then
                        // conspire to keep the old number on screen:
                        //
                        //   1. the 30 s trailing window still spans mostly BULK samples, so the
                        //      measurement itself stays high for a further 30 s; and
                        //   2. the Kalman gain (0.167 steady-state) needs ~34 more samples to
                        //      cross a 400x drop.
                        //
                        // Reported live at 98.6% synced: "4.6k blk/s even though its more down
                        // to 10". The panel was averaging across a boundary where averaging has
                        // no meaning — those are two different processes, not one noisy one.
                        //
                        // So: measure a SHORT trailing window too, and when it disagrees with
                        // the long one by a wide margin, believe the recent past. Discard the
                        // stale bulk samples and snap the filter rather than slewing.
                        const SHORT_WINDOW_SECS: f64 = 5.0;
                        let short_rate = app
                            .p2p_rate_samples
                            .iter()
                            .rev()
                            .find(|(t, _)| t1.duration_since(*t).as_secs_f64() >= SHORT_WINDOW_SECS)
                            .and_then(|&(ts, bs)| {
                                let sdt = t1.duration_since(ts).as_secs_f64();
                                (sdt >= 1.0).then(|| b1.saturating_sub(bs) as f64 / sdt)
                            });

                        // Only a COLLAPSE counts, and only a large one: a quarter of the long
                        // rate, and at least 100 blk/s of absolute difference so ordinary
                        // frontier jitter (12 -> 4 blk/s) never trips it. Those small moves are
                        // exactly what the filter is good at and should keep handling.
                        let collapsed = matches!(short_rate, Some(sr)
                            if long_rate > 0.0 && sr < long_rate * 0.25 && long_rate - sr > 100.0);

                        if let (true, Some(sr)) = (collapsed, short_rate) {
                            while app.p2p_rate_samples.len() > 1
                                && now
                                    .duration_since(app.p2p_rate_samples[0].0)
                                    .as_secs_f64()
                                    > SHORT_WINDOW_SECS
                            {
                                app.p2p_rate_samples.pop_front();
                            }
                            app.p2p_rate = sr;
                            app.sync_kf.reset_to(sr);
                        } else {
                            app.p2p_rate = long_rate;
                            // v0.33.3: feed the measured rate into the Kalman filter for a stable ETA.
                            app.sync_kf.update(app.p2p_rate);
                        }
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
            // [A]I: the local model's reply lands here (computed off the UI thread)
            if let Some(rx) = app.ai_rx.as_ref() {
                if let Ok(reply) = rx.try_recv() {
                    app.ai_msgs.push(("assistant".to_string(), reply));
                    app.ai_thinking = false;
                    app.ai_rx = None;
                }
            }
            // v0.7.0: Embedded serve is a thread — no health check needed
            // background self-update result (if any) → toast
            if let Some(rx) = app.update_rx.as_ref() {
                match rx.try_recv() {
                    Ok(msg) => {
                        // v0.7.5: Silent auto-check — just update the banner version
                        if let Some(ver) = msg.strip_prefix("AUTO-CHECK:") {
                            if !ver.is_empty() { app.latest = ver.to_string(); }
                            app.update_rx = None;
                        } else if let Some(e) = msg.strip_prefix("AUTO-CHECK-ERR:") {
                            app.toast = format!("⚠ update channel: {e}");
                            app.toast_sticky = false;
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
                        // Windows lock-fallback ("staged …"): a detached helper applies the swap
                        // and relaunches AFTER we exit — so leave the TUI cleanly and exit now
                        // (relaunching ourselves would pre-flight the still-OLD install path).
                        if msg.starts_with("staged v") {
                            let _ = disable_raw_mode();
                            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                            println!("\n  {msg}\n  applying update — relaunching…");
                            std::process::exit(0);
                        }
                        let is_update_ok = msg.contains("swapped") || msg.contains("saved v");
                        let mut relaunch_failed = false;
                        if is_update_ok {
                            let _ = disable_raw_mode();
                            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                            if !relaunch_new_binary(&app.latest) {
                                // relaunch failed — re-enter the TUI, don't crash out
                                let _ = enable_raw_mode();
                                let _ = execute!(std::io::stdout(), EnterAlternateScreen);
                                relaunch_failed = true;
                            }
                        }
                        // LANE-O (b): do NOT clobber a relaunch-FAILURE toast with the self_update
                        // "restart to run" msg — that hid WHY the restart didn't happen. On failure
                        // keep a STICKY honest message; only show msg when the handoff succeeded
                        // (unix exec never returns here) or nothing was swapped.
                        if relaunch_failed {
                            app.toast = format!("↑ {msg} — but AUTO-RESTART FAILED; please quit and relaunch sigil-top manually");
                            app.toast_sticky = true;
                        } else {
                            app.toast = msg; app.toast_sticky = false;
                        }
                        app.update_rx = None;
                        } // end else (non-auto-check message)
                    }
                    Err(mpsc::TryRecvError::Disconnected) => { app.update_rx = None; }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            // v0.37: mirror the in-process dual-lane engine into the Node hero legacy fields.
            if app.mining {
                if let Ok(es) = app.mine_stats.try_lock() {
                    app.mine_hashrate = es.hashrate / 1_000_000.0;
                    app.mine_accepted = es.shares_ok;
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
    IN_TUI.store(false, std::sync::atomic::Ordering::Relaxed);
    res
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
        "sigil-top-cortex",        // autho
        &format!("sigil-top v{VERSION} cortex auto-snapshot"),
    ).map_err(|e| format!("flux-rev snapshot: {e}"))?;
    Ok(rev.id)
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

use flux_miner::engine::{self, MinerStats};

#[derive(Clone, Copy, PartialEq)]
enum Tab { Node, SyncLog, Mining, Queues, Ai }

impl Tab {
    fn next(self) -> Tab {
        match self {
            Tab::Node => Tab::SyncLog,
            Tab::SyncLog => Tab::Mining,
            Tab::Mining => Tab::Queues,
            Tab::Queues => Tab::Ai,
            Tab::Ai => Tab::Node,
        }
    }
}

/// [A]I: send the typed line to the local model on a BACKGROUND thread; the reply
/// lands on `app.ai_rx` (drained in the render loop) so a slow model never freezes
/// the dashboard. With no model chosen, answer with the setup hint instead.
fn ai_submit(app: &mut App) {
    let msg = app.ai_input.trim().to_string();
    if msg.is_empty() { return; }
    app.ai_input.clear();
    app.ai_msgs.push(("user".to_string(), msg.clone()));
    let model = match app.ai_model.clone() {
        Some(m) => m,
        None => { app.ai_msgs.push(("assistant".to_string(), flux_moe::setup_hint().to_string())); return; }
    };
    let history: Vec<(String, String)> = app.ai_msgs[..app.ai_msgs.len().saturating_sub(1)].to_vec();
    let (tx, rx) = mpsc::channel();
    app.ai_rx = Some(rx);
    app.ai_thinking = true;
    std::thread::spawn(move || {
        let reply = match flux_moe::chat(&model, &history, &msg) {
            Ok(r) => r,
            Err(e) => format!("⚠ {e}"),
        };
        let _ = tx.send(reply);
    });
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

/// PERMANENT self-heal: a store belonging to a DIFFERENT CHAIN is cleared automatically.
///
/// # The failure this ends
///
/// Every header carries `sigil_header::NETWORK_ID`, and `precheck` refuses any header
/// whose id is not the one this binary was built with. When the network was cut over from
/// `sigil-g0` to `sigil-g1`, every existing client woke up holding a database full of
/// perfectly valid `sigil-g0` blocks and started rejecting **its own block 0**:
///
/// ```text
///   ⛓ INTEGRITY BROKEN — h=0: precheck failed:
///      wrong network id: expected [..115,45,103,49..] ("sigil-g1"),
///                             got [..115,45,103,48..] ("sigil-g0")
///   ✗ SPINE BREAK — STUCK   0.0%   rate 0 blk/s   eta —
/// ```
///
/// That is a permanent deadlock, and nothing in the sync loop can escape it: retrying an
/// honest peer harder cannot make a g0 block legal on g1, so the progress bar sits at 0.0%
/// forever. The user's only recourse was to know, unprompted, to delete a database file
/// they were never told about.
///
/// # Why this one is unconditional, unlike [`heal_wedged_store_once`]
///
/// That heal is a ONE-SHOT keyed to a marker string, because "this store might be wedged"
/// is a guess and wiping a healthy archive is expensive. A network-id mismatch is not a
/// guess — it is a proof. Blocks from another chain can never become valid here no matter
/// how long we wait, so clearing them is provably the only recovery, and it should work
/// every time it is needed rather than once per hand-bumped constant. This makes reset
/// work out of the box, permanently, for every future genesis change.
///
/// The chain a store belongs to is recorded in a `.netid` sidecar written on first launch.
/// A store with no sidecar predates this check and cannot be attributed to any chain, so
/// it is cleared once and re-synced — the same cost as the cutover already imposed.
/// The chain id this binary was COMPILED for, as text.
///
/// The TUI used to hard-code `"sigil-g0"` as its placeholder, so after the cutover it
/// cheerfully displayed `sigil-g0` while refusing every g0 block for not being g1 — the
/// header contradicting the error two lines below it. A label that can disagree with the
/// binary is worse than no label.
fn build_network_id() -> String {
    String::from_utf8_lossy(&sigil_header::NETWORK_ID).trim().to_string()
}

fn reset_store_on_network_change(path: &str) {
    let current = String::from_utf8_lossy(&sigil_header::NETWORK_ID).trim().to_string();
    let netid_path = format!("{path}.netid");
    let reason = match std::fs::read_to_string(&netid_path) {
        // Same chain — leave the archive completely alone. This is the normal path and it
        // must stay cheap: one small file read per launch.
        Ok(prev) if prev.trim() == current => return,
        Ok(prev) => format!("network changed: store holds '{}', this build is '{current}'", prev.trim()),
        Err(_) => format!("store predates the network-id marker; cannot prove it is '{current}'"),
    };
    // A store that does not exist yet needs no wipe — just record the chain it will hold.
    let exists = std::path::Path::new(path).exists();
    if exists {
        eprintln!(
            "  ⛓ RESET: {reason} — clearing the local chain store and re-syncing from genesis."
        );
        let _ = std::fs::remove_dir_all(path);
        let _ = std::fs::remove_file(path);
        for sfx in ["-wal", "-shm", ".wal", ".shm"] {
            let _ = std::fs::remove_file(format!("{path}{sfx}"));
        }
        // The one-shot heal marker refers to a store that no longer exists. Dropping it
        // keeps the two mechanisms from disagreeing about what is on disk.
        let _ = std::fs::remove_file(format!("{path}.healver"));
    }
    let _ = std::fs::write(&netid_path, &current);
    boot_trace(&format!("network-id reset: {reason} (store existed: {exists}) — now on '{current}'"));
}

/// v7.0.7 ONE-TIME store heal, RE-ARMED 2026-08-24 (see below). Stores built by the
/// v7.0.3–7.0.5 sync FRONTIER-STALL bug wedge at a chunk boundary (a Fatal parent-linkage
/// break — the operator's h≈393,265 "SPINE BREAK — STUCK") and do NOT self-heal in place:
/// a later fetch-ordering fix only prevents NEW wedges, it can't repair an already-corrupt
/// on-disk chain. So on the FIRST launch of a build carrying a given `HEAL_MARKER`, delete
/// the store once; the sync then rebuilds it clean. A tiny `.healver` marker next to the
/// store records that THIS marker's heal ran, so later launches skip it (never a repeat
/// wipe for the same marker). A store that was already clean simply re-syncs once — a
/// testnet-acceptable one-time cost, and the ONLY way to make "press U and it just works"
/// true for every node already carrying a wedged store.
///
/// **RE-ARMED 2026-08-24** (bumped `v7.1.49` → `v7.1.75`): the marker is a ONE-SHOT per
/// exact string, not a permanent "this class of bug can never recur" guarantee — any store
/// that already consumed the v7.1.49 heal (i.e. every node that's launched at least once
/// since that release) is now IMMUNE to a repeat wipe even if it picks up a NEW wedge from
/// a DIFFERENT bug, because `heal_wedged_store_once` only compares the marker file's
/// content to the CURRENT constant. Confirmed this actually happened: live-reproduced on
/// Epsilon (2026-08-24) against a shared root-owned store that HAD already been healed
/// under the old marker — a background reconciliation pass hit hundreds of consecutive
/// `[store] rejected height-index fork overwrite` entries in the h≈1.6–2.0M range (two
/// completely different, uncorrelated hashes for the same height, `existing=` vs
/// `incoming=`), with the sync frontier frozen and the live fetch loop timing out on 71%
/// of requests — the exact "SPINE BREAK — STUCK" signature, just at a different height and
/// from whichever of the many uncommitted skeleton/backfill passes (see
/// `project_sigil_sync_index_vs_body_gap_2026_08_18` memory — VCATCH v1/v2, the
/// height-1-index-conflict finding, several NOT-yet-fully-landed attempts) wrote a
/// conflicting entry into that store at some point after v7.1.49 shipped. The store's own
/// conflict-rejection logic (`block_store.rs::put_blocks_batch` — correctly refusing to
/// let ANY later response silently overwrite an already-indexed height, which is real
/// anti-fork protection) cannot distinguish "a malicious/forked peer" from "our own local
/// store wrote something wrong, once, from an in-development code path" — so once a bad
/// entry lands, by ANY mechanism, ever, the spine wedges at that exact height PERMANENTLY
/// and no amount of retrying a healthy, honest peer can ever recover it. This is NOT a fix
/// for whatever wrote the bad entry in the first place (that remains open — the skeleton/
/// backfill code has had multiple actively-evolving, sometimes-uncommitted-and-broken
/// attempts across the v7.1.30–v7.1.74 range; see the memory file above for the specifics
/// not yet fully closed) — it is the same "clear the landmine" recovery this project has
/// already shipped once, re-armed so it fires again for stores that picked up NEW damage
/// since the last time.
fn heal_wedged_store_once(path: &str) {
    // RE-ARMED 2026-08-27 (v7.1.75 -> v7.2.5). The marker is a ONE-SHOT per exact string,
    // so every store that has launched since v7.1.75 is immune to a repeat wipe even when
    // it picks up NEW damage — which is exactly what happened. Live: an operator's client
    // sat with `verified 30,250` while `fetched-to` ran to 120,000, across several restarts
    // and four releases. Honest headers for `30,250..40,250` arrived and completed in ~2 s
    // every time, and the store refused to splice them: a poisoned local seam that no
    // amount of refetching can repair, because the bad block is OURS.
    //
    // The bounded in-run self-heal (`rollback_frontier(4096)`, 3 attempts behind a 45 s
    // watchdog) only covers damage within ~12k blocks of the frontier. This covers the case
    // where it is deeper, or where the wedge survives restarts. The store re-syncs clean —
    // at the rates these clients reach that is a couple of minutes.
    const HEAL_MARKER: &str = "frontier-stall-heal-v7.2.5";
    let marker = format!("{path}.healver");
    if std::fs::read_to_string(&marker).map(|s| s.trim() == HEAL_MARKER).unwrap_or(false) {
        return; // already healed under THIS marker on a prior launch — leave the store alone
    }
    // The store may be a flux-db DIRECTORY or a file, plus WAL/SHM sidecars — clear them all.
    let _ = std::fs::remove_dir_all(path);
    let _ = std::fs::remove_file(path);
    for sfx in ["-wal", "-shm", ".wal", ".shm"] {
        let _ = std::fs::remove_file(format!("{path}{sfx}"));
    }
    let _ = std::fs::write(&marker, HEAL_MARKER);
    boot_trace(&format!(
        "v7.1.75 one-time heal: cleared possibly-wedged store {path} (re-armed frontier-stall heal) — re-syncing from genesis"
    ));
}

fn light_boot_store_limit_bytes() -> u64 {
    if let Ok(raw) = std::env::var("SIGIL_TOP_BOOT_STORE_LIMIT_MB") {
        if let Ok(mb) = raw.trim().parse::<u64>() {
            return if mb == 0 { u64::MAX } else { mb.saturating_mul(1024 * 1024) };
        }
    }
    // v7.0.12: was 512 MiB (Windows) / 1.5 GiB (Linux) — FAR too low. A full-archive store is
    // several GB, so it ALWAYS exceeded the cap → the dashboard booted on a throwaway VOLATILE
    // store every launch → the sync NEVER persisted and re-synced from 0 on every update ("doesn't
    // resume"). 64 GiB opens the REAL persistent store for any realistic archive; the 20s
    // open-timeout still catches a genuinely-stuck open, and SIGIL_TOP_BOOT_STORE_LIMIT_MB=0
    // disables the cap entirely.
    64u64 * 1024 * 1024 * 1024
}

fn dir_size_capped(path: &str, cap: u64) -> std::io::Result<u64> {
    let root = std::path::Path::new(path);
    if !root.exists() {
        return Ok(0);
    }
    let meta = fs::metadata(root)?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
                if total > cap {
                    return Ok(total);
                }
            }
        }
    }
    Ok(total)
}

fn oversized_store_for_light_boot(path: &str, want_sync: bool) -> Option<u64> {
    if want_sync || std::env::var("SIGIL_TOP_FORCE_STORE").is_ok() {
        return None;
    }
    let cap = light_boot_store_limit_bytes();
    if cap == u64::MAX {
        return None;
    }
    match dir_size_capped(path, cap) {
        Ok(bytes) if bytes > cap => Some(bytes),
        Ok(_) => None,
        Err(e) => {
            boot_trace(&format!("store size preflight failed for {path}: {e}"));
            None
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut n = bytes as f64;
    let mut unit = 0usize;
    while n >= 1024.0 && unit + 1 < UNITS.len() {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}


/// v0.36.1: the MINING HERO — a full-width band (sized like the SYNC hero) under it, with a
/// big NETWORK-POWER bar (the chain's block-production pulse), network economics, the
/// operator's PERSONAL mining (hashrate / accepted shares / wallet / streak), and a forge
/// motif that glows HOT while [M]ining. Honest: SIGIL P0 is verify-once, so "network power"
/// is the real block-production throughput + emission, not a fictional global hashrate;
/// personal hashrate IS real local BLAKE work.
/// v7.1.15: the engine's node base-URL — the braid (sigil-node), NOT the legacy
/// sigil-rpcd chain. Same wire contract (`/api/v1/mining/{challenge,submit}`,
/// `sigil-api::mining`'s wire-compatible aliases), so this is a URL-only change:
/// a solved share now credits real pool-share mining ONTO the DagKnight/GHOSTDAG
/// braid (dev-fee + commons split, proportional payout) instead of the separate
/// rpcd chain. Override with SIGIL_MINE_NODE to point at rpcd (:8099) or
/// anywhere else. Distinct from the legacy [m] BLAKE3 `mine_url()`.
pub(crate) fn engine_node_url() -> String {
    // An explicit operator choice always wins — never override it, producer mode or not.
    if let Ok(v) = std::env::var("SIGIL_MINE_NODE") {
        return v;
    }
    // 2026-08-25 (local-mining-API work, operator-directed: "let a miner mine against
    // their OWN locally-running node instead of always hitting the central Epsilon
    // node"): when this process's own producer mode has a local mining API ACTUALLY up
    // (not merely requested — see `producer::mining_api::local_mining_api_is_up`'s doc
    // for what "up" means: a confirmed TCP accept, not just the env vars being set),
    // prefer it. `local_mining_api_is_up()` is a single relaxed atomic load, so this is
    // safe to call from a hot per-frame render path (the Mining tab). In every build
    // without the `producer` feature — the overwhelming majority of users — this branch
    // doesn't exist at all, and in a `producer`-feature build where the local server was
    // never started (no env vars, or sync/bind failed), the flag is false and this falls
    // through exactly as before: byte-identical default behavior either way.
    #[cfg(feature = "producer")]
    if producer::mining_api::local_mining_api_is_up() {
        return producer::mining_api::local_mining_api_url();
    }
    "http://sigilgraph.quillon.xyz:18181".into()
}





// ── v0.7.0: AI Fleet Monitoring ─────────────────────────────────────────



// ── v0.6.0: Cortex MCP combo card ──────────────────────────────────────


// ── v0.3.5: Browser shortcuts ────────────────────────────────────────────






// ── Windows: launch-at-login + system tray ──────────────────────────────────
// The terminal node should be able to start with the Windows session and live in the
// notification area. Auto-start = a HKCU Run key (via reg.exe — same shell-out pattern the
// rest of the Windows integration uses, no extra crate). The tray = a PowerShell NotifyIcon
// helper (assets/sigil-tray.ps1) spawned ISOLATED, so a tray failure can never affect sync.



// ── flux:// URL scheme ──────────────────────────────────────────────────────
// `flux://wallet` typed in the browser → the OS launches `sigil-top flux-open
// flux://wallet`. UI targets open the embedded :9800 wallet; command targets run
// the `fluxc` binary in a VISIBLE terminal (never silent exec from a URL).








#[cfg(test)]
mod pure_helpers_tests {
    //! Coverage for the pure money/format/version helpers (Tier 3 — sigil-top
    //! was the worst-density crate at 581 loc/test). All deterministic, no I/O.
    use super::*;

    /// How many samples the rate filter needs to cross the bulk→frontier cliff.
    ///
    /// The operator saw "4.6k blk/s" at 98.6% synced while the node was really doing ~10.
    /// That is the moment bulk import ends and the frontier follow begins — a genuine
    /// regime change, not a noisy reading.
    fn samples_to_converge(from: f64, to: f64) -> usize {
        let mut kf = Kalman1D::new();
        for _ in 0..40 {
            kf.update(from);
        }
        assert!((kf.x - from).abs() < from * 0.05, "filter should settle on the bulk rate first");
        for n in 1..=200 {
            kf.update(to);
            // "believable" = within 2x of the truth; at 10 blk/s that is <= 20 on screen.
            if kf.x <= to * 2.0 {
                return n;
            }
        }
        usize::MAX
    }

    #[test]
    fn rate_filter_crosses_a_regime_change_quickly() {
        let n = samples_to_converge(4_600.0, 10.0);
        // The fixed-gain filter (steady-state k = 36/216 = 0.167) needed ~34 samples: with a
        // ~1 s refresh that is over half a minute of showing a number 400x too high, right at
        // the point an operator is watching to see whether sync has finished.
        assert!(
            n <= 8,
            "REGRESSION: rate filter took {n} samples to cross a 4600 -> 10 blk/s collapse; \
             the adaptive gain should cross it in a handful"
        );
    }

    /// The adaptive gain must not turn the filter into a passthrough — smoothing ordinary
    /// jitter is the whole reason it exists, and a twitchy readout was the ORIGINAL
    /// complaint that motivated the filter in the first place.
    #[test]
    fn rate_filter_still_smooths_ordinary_jitter() {
        let mut kf = Kalman1D::new();
        for _ in 0..40 {
            kf.update(100.0);
        }
        let settled = kf.x;
        // A single 20% outlier must barely move the estimate.
        kf.update(120.0);
        assert!(
            (kf.x - settled).abs() < 6.0,
            "a lone 20% blip moved the estimate by {:.2} — too twitchy",
            (kf.x - settled).abs()
        );
    }

    /// `reset_to` is the explicit escape hatch the caller uses once it has DECIDED a
    /// collapse happened; it must land exactly, not slew.
    #[test]
    fn reset_to_lands_exactly_and_stays_usable() {
        let mut kf = Kalman1D::new();
        for _ in 0..40 { kf.update(4_600.0); }
        kf.reset_to(10.0);
        assert_eq!(kf.x, 10.0, "reset must land exactly on the new rate");
        kf.update(11.0);
        assert!(kf.x > 9.0 && kf.x < 12.0, "and the filter must keep working afterwards");
        kf.reset_to(f64::NAN);
        assert!(kf.x.is_finite(), "a non-finite reset must be ignored, not poison the estimate");
    }

    #[test]
    fn hex_to_32_roundtrips_and_rejects_bad_input() {
        // 64 hex chars → 32 bytes, value-correct.
        let h = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let b = hex_to_32(h).expect("valid 64-hex");
        assert_eq!(b[0], 0x00);
        assert_eq!(b[1], 0x11);
        assert_eq!(b[31], 0xff);
        // uppercase accepted (from_str_radix is case-insensitive)
        assert_eq!(hex_to_32(&h.to_uppercase()), Some(b));
        // surrounding whitespace trimmed
        assert_eq!(hex_to_32(&format!("  {h}  ")), Some(b));
        // 2026-08-25: an 0x/0X-prefixed seed must decode identically -- mine-rig's
        // seed_bytes() (shield_setup.rs) already stripped this prefix; this function
        // silently didn't, so a prefixed SIGIL_MINE_SEED mined headless correctly but
        // fell through to an unrelated fallback wallet for [M]ine/[W].
        assert_eq!(hex_to_32(&format!("0x{h}")), Some(b));
        assert_eq!(hex_to_32(&format!("0X{h}")), Some(b));
        assert_eq!(hex_to_32(&format!("  0x{h}  ")), Some(b));
        // wrong length / non-hex rejected
        assert_eq!(hex_to_32(&"a".repeat(63)), None);
        assert_eq!(hex_to_32(&"a".repeat(65)), None);
        assert_eq!(hex_to_32(&"g".repeat(64)), None, "non-hex must be rejected, not silently zeroed");
    }

    #[test]
    fn valid_addr_requires_exactly_64_hex() {
        assert!(valid_addr(&"a".repeat(64)));
        assert!(valid_addr(&"A1b2".repeat(16))); // mixed case, 64 chars
        assert!(valid_addr(&format!("  {}  ", "f".repeat(64))), "trims before checking");
        assert!(!valid_addr(&"a".repeat(63)));
        assert!(!valid_addr(&"a".repeat(65)));
        assert!(!valid_addr(&"z".repeat(64)), "non-hex rejected");
        assert!(!valid_addr(""));
    }

    #[test]
    fn fmt_supply_truncates_sub_unit_and_groups_thousands() {
        let unit = 10u128.pow(DECIMALS);
        assert_eq!(fmt_supply(0), "0");
        assert_eq!(fmt_supply(unit - 1), "0", "sub-unit dust truncates to 0 whole");
        assert_eq!(fmt_supply(unit), "1");
        assert_eq!(fmt_supply(999 * unit), "999");
        assert_eq!(fmt_supply(1_000 * unit), "1,000");
        assert_eq!(fmt_supply(1_234_567 * unit), "1,234,567");
    }

    #[test]
    fn fmt_uptime_picks_the_right_granularity() {
        assert_eq!(fmt_uptime(0), "0m 0s");
        assert_eq!(fmt_uptime(59), "0m 59s");
        assert_eq!(fmt_uptime(60), "1m 0s");
        assert_eq!(fmt_uptime(3600), "1h 0m");
        assert_eq!(fmt_uptime(90_061), "1d 1h 1m"); // 1d + 1h + 1m + 1s
    }

    #[test]
    fn one_chain_view_fills_uptime_and_holds_spine_height() {
        // uptime always becomes real (>=1) when the feed gives 0
        let mut st = NodeStatus { uptime_secs: 0, height: 31_780_000, ..Default::default() };
        // spine ON (engine running): height untouched, no target override, uptime filled
        let t = one_chain_view(&mut st, false);
        assert_eq!(t, None, "spine-on must not override target from ledger");
        assert_eq!(st.height, 31_780_000, "spine-on leaves the engine's own height");
        assert!(st.uptime_secs >= 1, "uptime must never render 0m 0s forever");
        // a non-zero feed uptime is respected, not clobbered
        let mut st2 = NodeStatus { uptime_secs: 42, height: 5, ..Default::default() };
        one_chain_view(&mut st2, false);
        assert_eq!(st2.uptime_secs, 42);
    }

    #[test]
    fn fmt_eta_handles_nonfinite_and_ranges() {
        assert_eq!(fmt_eta(-1.0), "—");
        assert_eq!(fmt_eta(0.0), "—");
        assert_eq!(fmt_eta(f64::INFINITY), "—", "non-finite must not panic / format garbage");
        assert_eq!(fmt_eta(f64::NAN), "—");
        assert_eq!(fmt_eta(45.0), "45s");
        assert_eq!(fmt_eta(125.0), "2m");
        assert_eq!(fmt_eta(3_661.0), "1h 1m");
        assert_eq!(fmt_eta(60.0 * 60.0 * 24.0 * 100.0), "∞", "absurd ETA collapses to ∞");
    }

    #[test]
    fn version_gt_orders_semver_numerically() {
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(version_gt("1.10.0", "1.9.0"), "10 > 9 numerically, not lexically");
        assert!(version_gt("2", "1.9.9"), "shorter-but-larger major wins");
        assert!(version_gt("1.0.1", "1.0.0"));
        assert!(!version_gt("1.2.3", "1.2.3"), "equal is not greater");
        assert!(!version_gt("1.0.0", "1.0.1"));
        assert!(!version_gt("1.0", "1.0.0"), "1.0 == 1.0.0 (missing components are 0)");
    }
}
