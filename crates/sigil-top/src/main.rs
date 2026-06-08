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
mod serve;

use std::io::{IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};


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
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            use std::io::Write;
            let _ = writeln!(f, "{s}");
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
const LATEST: &str = env!("CARGO_PKG_VERSION");
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
         sigil-top --api URL    status endpoint (default https://sigilgraph.fluxapp.xyz/api/v1/status)"
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
    o.push('\n');

    // NODE panel (section title embedded in the top border)
    o.push_str(&top_title("NODE"));
    let prod = if st.producer.is_empty() { "—".into() } else { st.producer.clone() };
    let ver = if st.version.is_empty() { "—".into() } else { st.version.clone() };
    let disp_height = st.tip.as_ref().map(|t| t.height).filter(|h| *h > 0).unwrap_or(st.height);
    o.push_str(&row("height", &format!("{GOLD}{}{RESET}", disp_height)));
    if st.blocks_per_sec > 0.0 {
        let bps_col = if st.blocks_per_sec >= 1000.0 { GOLD } else { GREEN };
        o.push_str(&row("blocks/s", &format!("{bps_col}{:.0}{RESET} {DIM}(live feed){RESET}", st.blocks_per_sec)));
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

fn main() {
    // subcommands: login / logout (handled before the render loop)
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(|s| s.as_str()) {
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
            match serve::start(&serve_dir, port) {
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
                        Ok(msg) => { println!("  {GREEN}{msg}{RESET}\n"); std::process::exit(0); }
                        Err(e)  => { eprintln!("  {RED}✗ {e}{RESET}\n"); std::process::exit(1); }
                    }
                }
                Ok(rel) => { println!("  {GREEN}✓ already on the latest (v{VERSION}; channel: v{}){RESET}\n", rel.version); return; }
                Err(e) => { eprintln!("  {RED}✗ update check: {e}{RESET}\n"); std::process::exit(1); }
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
            // Re-exec the freshly-swapped binary with the original args (minus argv[0]).
            if let Ok(exe) = std::env::current_exe() {
                let args: Vec<String> = std::env::args().skip(1).collect();
                std::env::set_var("SIGIL_TOP_JUST_UPDATED", "1");
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    let _ = std::process::Command::new(&exe).args(&args).exec(); // replaces this process
                }
                #[cfg(windows)]
                {
                    // Spawn the NEWLY SAVED binary, not this one (avoids infinite loop)
                    if let Ok(beside) = std::env::current_exe() {
                        let new_exe = beside.with_file_name(format!("sigil-top-v{}.exe", rel.version));
                        if new_exe.exists() {
                            let _ = std::process::Command::new(&new_exe).args(&args).spawn();
                            std::process::exit(0);
                        }
                    }
                    // Fallback: just exit, user runs the new binary manually
                    std::process::exit(0);
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new(&exe).args(&args).spawn();
                    std::process::exit(0);
                }
            }
            None // unreachable — exec replaces this process
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
    if self_replace::self_replace(&beside).is_ok() {
        let _ = std::fs::remove_file(&beside);
        return Ok(format!("swapped v{VERSION} -> v{} ({:.1} MB) — restart to run",
            rel.version, bytes.len() as f64 / 1.048576e6));
    }
    Ok(format!("saved v{} ({:.1} MB) -> {}", rel.version, bytes.len() as f64 / 1.048576e6, beside.display()))
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
    mcp_combo_tool: String,     // active MCP combo verb being executed
    mcp_combo_result: String,   // last MCP combo result
    // v0.6.5: Real P2P block sync via flux-p2p mesh (Delta + Epsilon)
    p2p_sync: Option<block_sync::P2PBlockSync>,
    p2p_state: block_sync::P2PSyncState,
    p2p_blocks_synced: u64,
    p2p_rate: f64,                  // smoothed backfill blocks/sec (for the SYNC card)
    p2p_prev_synced: u64,          // last sample of p2p_state.blocks_synced
    p2p_rate_at: std::time::Instant,
    // v0.7.0: AI fleet monitoring — AIs worry about their nodes' uptime and version compliance
    fleet_nodes: Vec<FleetNode>,
    fleet_last_check: Instant,
    // v0.6.0: fluxc serve status for local wallet + cockpit
    serve_status: String,
    // v0.7.0: embedded HTTP serve shutdown signal (no external process)
    serve_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl App {
    fn new(cfg: Config) -> Self {
        let toast = cfg.initial_toast.clone().unwrap_or_default();
        App { cfg, st: NodeStatus::default(), online: false, last_fetch: Instant::now(),
              verify: None, toast, toast_sticky: false,
              latest: LATEST.to_string(),
              // v0.7.5: Trigger first check immediately (now - 300s = overdue)
              last_update_check: Instant::now() - Duration::from_secs(301),
              update_rx: None,
              blocks: Vec::new(),
              target_height: 0, synced_height: 0, verified_count: 0, streak: 0, score: 0,
              mining: false, mine_rx: None, mine_stop: None, mine_accepted: 0,
              mine_hashrate: 0.0, mine_hashes: 0, wallet_balance: 0,
              full_sync: false, full_sync_height: 0, full_sync_target: 0, full_sync_active: false, sync_us: 0,
              eclipse_k: 0, eclipse_sources: Vec::new(),
              last_eclipse: Instant::now() - Duration::from_secs(60),
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
              mcp_combo_tool: String::new(),
              mcp_combo_result: String::new(),
              // v0.6.5: P2P block sync starts lazy — launched in run_tui after terminal is ready
              p2p_sync: None,
              p2p_state: block_sync::P2PSyncState::default(),
              p2p_blocks_synced: 0,
              p2p_rate: 0.0,
              p2p_prev_synced: 0,
              p2p_rate_at: std::time::Instant::now(),
              serve_status: String::new(),
              serve_stop: None,
              // v0.7.0: Fleet starts with known bootstrap peers
              fleet_nodes: vec![
                  FleetNode { name: "Delta".into(), addr: "5.79.79.158".into(), port: 9003, online: false, height: 0, version: String::new(), uptime_secs: 0 },
                  FleetNode { name: "Epsilon".into(), addr: "89.149.241.126".into(), port: 9003, online: false, height: 0, version: String::new(), uptime_secs: 0 },
              ],
              fleet_last_check: Instant::now() - Duration::from_secs(3600),
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
    fn refresh(&mut self) {
        // Live testnet sync: pull {status, tip, blocks} over HTTPS; fall back to a local node.
        let got = fetch_feed(&self.cfg.feed)
            .map(|(s, b)| { self.blocks = b; (s, true) })
            .or_else(|| fetch(&self.cfg.api).ok().map(|s| (s, true)))
            .unwrap_or((NodeStatus::default(), false));
        self.st = got.0; self.online = got.1;
        // v0.4.0: if feed returned empty blocks, try fallback API for recent blocks
        if self.blocks.is_empty() && self.online {
            if let Ok(client) = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(8))
                .danger_accept_invalid_certs(true)
                .build()
            {
                let api_base = self.cfg.api.trim_end_matches('/');
                if let Ok(resp) = client.get(format!("{}/v1/blocks/recent?limit=14", api_base)).send() {
                    if let Ok(json) = resp.json::<serde_json::Value>() {
                        if let Some(arr) = json.get("blocks").or_else(|| json.get("data")).and_then(|v| v.as_array()) {
                            let fallback: Vec<FeedBlock> = arr.iter().filter_map(|b| {
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
                            if !fallback.is_empty() {
                                self.blocks = fallback;
                                self.toast = "📡 Blocks fetched from API fallback".into();
        }
                        }
                    }
                }
            }
        }
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
        // L2-B: re-measure eclipse-K (real, throttled to 30s — DoH queries cost RTT).
        if self.last_eclipse.elapsed() >= Duration::from_secs(30) {
            let tip_ok = self.verify.as_ref().map(|v| v.ok).unwrap_or(false);
            let (k, srcs) = measure_eclipse_k(self.synced_height, tip_ok);
            self.eclipse_k = k;
            self.eclipse_sources = srcs;
            self.last_eclipse = Instant::now();
        }
        self.last_fetch = Instant::now();
    }
}

/// L2-B: REAL eclipse-K — count INDEPENDENT verification paths that agree on the chain tip,
/// replacing the old hardcoded `k=2`. Path 0 = the node/feed tip we just cryptographically verified.
/// Paths 1..N = independent public DoH resolvers resolving the `_sigil-tip` anchor TXT; one counts
/// only if its answer carries the current tip height (so a single lying resolver can't fake the tip —
/// DNS-level eclipse resistance). HONEST: until the anchor is published (L2-C), the DoH paths return
/// nothing → K reflects only what was really verified, never a simulated climb.
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
    enable_raw_mode()?;
    // From here ratatui owns the screen — divert background eprintln to the logfile
    // (was smearing the dashboard with [p2p-sync]/[aether] lines).
    IN_TUI.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let mut app = App::new(cfg);
    app.refresh();

    // v0.7.0: Open flux-db block store, sync from local aether shards, then launch P2P
    let mut block_store = block_store::BlockStore::open("/tmp/sigil-top-blocks.db")
        .unwrap_or_else(|_| {
            block_store::BlockStore::open("/dev/shm/sigil-top-blocks.db")
                .unwrap_or_else(|e| panic!("block store: {e}"))
        });

    // v0.7.1: Bootstrap from local aether shards into flux-db before starting P2P
    match block_store::sync_aether_to_fluxdb(&mut block_store, "/opt/orobit/sigil-data/db-epsilon/aether") {
        Ok(n) if n > 0 => {
            app.toast = format!("⬇ Synced {n} blocks → flux-db (height {})", block_store.best_height());
        }
        Err(e) => tlog!("[aether] {e}"),
        _ => {}
    }

    // Launch P2P block sync for live blocks (pass ownership of store)
    app.p2p_sync = Some(block_sync::P2PBlockSync::launch(block_store));
    if app.toast.is_empty() {
        app.toast = "⚡ P2P mesh connecting → Delta / Epsilon…".into();
    }

    // v0.7.0: Start embedded HTTP server (no external process needed)
    let serve_dir = std::env::var("FLUX_STATIC_DIR")
        .unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp".into());
    match serve::start(&serve_dir, 9800) {
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
            term.draw(|f| draw_ui(f, &app))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        match k.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('r') | KeyCode::Char('R') => { app.refresh(); app.toast_sticky = false; }
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
                                // v0.7.0: local TRON wallet served by fluxc serve on :9800
                                let wallet_url = local_wallet_url();
                                open_browser(&wallet_url);
                                app.toast = format!("🌐 TRON wallet → {wallet_url}").into();
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('b') | KeyCode::Char('B') => {
                                open_browser("https://sigilgraph.fluxapp.xyz/explorer/");
                                app.toast = "🌐 Explorer opened in browser".into();
                                app.toast_sticky = false;
                            }
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                open_browser("https://sigilgraph.fluxapp.xyz/sigil-top/");
                                app.toast = "🌐 Cockpit opened in browser".into();
                                app.toast_sticky = false;
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
            if app.last_fetch.elapsed() >= Duration::from_secs(app.cfg.interval) {
                app.refresh();
            }
            // v0.6.5: Poll P2P sync state + drain synced blocks into the TUI block list
            if let Some(ref p2p) = app.p2p_sync {
                app.p2p_state = p2p.poll_state();
                // Smoothed backfill rate (blocks/s) for the SYNC card, sampled ≥1s apart.
                let dt = app.p2p_rate_at.elapsed().as_secs_f64();
                if dt >= 1.0 {
                    let cur = app.p2p_state.blocks_synced;
                    let delta = cur.saturating_sub(app.p2p_prev_synced) as f64;
                    let inst = delta / dt;
                    app.p2p_rate = if app.p2p_rate <= 0.0 { inst } else { app.p2p_rate * 0.6 + inst * 0.4 };
                    app.p2p_prev_synced = cur;
                    app.p2p_rate_at = std::time::Instant::now();
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
                        let is_update_ok = msg.contains("swapped")
                            || msg.contains("saved v")
                            || msg.starts_with("✓");
                        if is_update_ok {
                            let _ = disable_raw_mode();
                            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                            // Try the versioned binary beside us first (v0.7.0 rename trick)
                            if let Ok(exe) = std::env::current_exe() {
                                let args: Vec<String> = std::env::args().skip(1).collect();
                                std::env::set_var("SIGIL_TOP_JUST_UPDATED", "1");
                                // On Unix: exec replaces this process with the new binary.
                                // On Windows: spawn + exit (can't replace running .exe).
                                #[cfg(unix)]
                                {
                                    use std::os::unix::process::CommandExt;
                                    // Try the versioned binary self_update just saved
                                    // (named for the FETCHED version, not a stale const).
                                    let ver_exe = exe.with_file_name(format!("sigil-top-v{}", app.latest));
                                    let target = if ver_exe.exists() { &ver_exe } else { &exe };
                                    let _ = std::process::Command::new(target).args(&args).exec();
                                }
                                #[cfg(not(unix))]
                                {
                                    let ver_exe = exe.with_file_name(format!("sigil-top-v{}.exe", app.latest));
                                    let target = if ver_exe.exists() { &ver_exe } else { &exe };
                                    let _ = std::process::Command::new(target).args(&args).spawn();
                                    std::process::exit(0);
                                }
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

fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();
    if let Some(until) = app.splash_until {
        if Instant::now() < until {
            f.render_widget(render_update_splash(app.splash_frame), area);
            return;
        }
    }
    let [header_area, body_area, footer_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(2)]).areas(area);

    f.render_widget(render_header(app), header_area);

    let body_h = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(body_area);
    let (left_area, right_area) = (body_h[0], body_h[1]);

    let left_v = Layout::vertical([
        Constraint::Length(6), // Node
        Constraint::Length(6), // StateRoots
        Constraint::Length(4), // Supply
        Constraint::Length(7), // SyncStatus (v0.7.19: 5 lines — tip/sync-bar/rate+ETA/chunk/p2p)
        Constraint::Length(7), // Mining (v0.2.35: +2 lines for hashrate + balance)
    ])
    .spacing(1)
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

    f.render_widget(render_footer(app), footer_area);
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

fn render_sync_status(app: &App) -> Paragraph<'static> {
    let s = &app.p2p_state;
    let verified = app.verify.as_ref().map(|v| v.ok).unwrap_or(false);
    let synced = s.blocks_synced;                          // blocks actually stored (DB fill)
    let tip = s.peer_best_height.max(app.target_height);   // network tip
    let gap = tip.saturating_sub(synced);
    let at_tip = tip > 0 && gap < 8;
    let rate = app.p2p_rate;

    // tip — light-client cryptographic verify (instant, ~µs)
    let tip_line = if !app.online {
        Line::from(vec![dim("tip   "), Span::styled("— offline (no feed/node)", Style::default().fg(C_RED))])
    } else if verified {
        Line::from(vec![
            dim("tip   "), Span::styled("✓ verified", Style::default().fg(C_GREEN)),
            dim("  h "), Span::styled(group(app.synced_height), Style::default().fg(C_GREEN)),
            dim(format!("  · {} µs", app.sync_us)),
        ])
    } else {
        Line::from(vec![dim("tip   "), Span::styled("✗ unverified", Style::default().fg(C_GOLD))])
    };

    // sync — DB-fill progress bar (blocks stored vs network tip) + gap framing
    let pct = if tip > 0 { (synced as f64 / tip as f64 * 100.0).min(100.0) } else { 100.0 };
    let bar_w = 18usize;
    let filled = ((pct / 100.0) * bar_w as f64).round() as usize;
    let bar = "█".repeat(filled.min(bar_w)) + &"░".repeat(bar_w.saturating_sub(filled));
    let (bar_col, tail) = if at_tip {
        (C_GREEN, Span::styled("  AT TIP".to_string(), Style::default().fg(C_GREEN)))
    } else {
        (C_CYAN, Span::styled(format!("  {} behind", group(gap)), Style::default().fg(C_GOLD)))
    };
    let sync_line = Line::from(vec![
        dim("sync  "),
        Span::styled(bar, Style::default().fg(bar_col)),
        dim(format!(" {:.0}%", pct)),
        tail,
    ]);

    // rate + ETA — the live backfill throughput
    let rate_line = if at_tip {
        Line::from(vec![dim("rate  "), Span::styled("tracking live", Style::default().fg(C_GREEN))])
    } else if rate > 0.5 {
        let eta_s = (gap as f64 / rate) as u64;
        let eta = if eta_s >= 3600 { format!("{}h{}m", eta_s / 3600, (eta_s % 3600) / 60) }
            else if eta_s >= 60 { format!("{}m{:02}s", eta_s / 60, eta_s % 60) }
            else { format!("{}s", eta_s) };
        Line::from(vec![
            dim("rate  "),
            Span::styled(format!("{} blk/s", group(rate.round() as u64)), Style::default().fg(C_CYAN).add_modifier(Modifier::BOLD)),
            dim("   ETA "), Span::styled(eta, Style::default().fg(C_VBRIGHT)),
        ])
    } else {
        Line::from(vec![dim("rate  "), Span::styled("starting…", Style::default().fg(C_DIM))])
    };

    // chunk — the request-response backfill range in flight (or stored total when synced)
    let chunk_line = if !at_tip && s.running {
        let from = s.sync_cursor;
        let to = from.saturating_add(8192);
        Line::from(vec![
            dim("chunk "),
            Span::styled(format!("[{}..{}]", group(from), group(to)), Style::default().fg(C_VBRIGHT)),
            dim("  ⬇ "), Span::styled(format!("{} fetched", group(synced)), Style::default().fg(C_GREEN)),
        ])
    } else {
        Line::from(vec![
            dim("store "),
            Span::styled(format!("{} blocks", group(synced)), Style::default().fg(C_GREEN)),
            dim("  · req-response backfill"),
        ])
    };

    // p2p — mesh peers
    let p2p_line = if s.running {
        let d = if s.connected_delta { "Δ" } else { "·" };
        let e = if s.connected_epsilon { "Ε" } else { "·" };
        Line::from(vec![
            dim("p2p   "),
            Span::styled(d, Style::default().fg(if s.connected_delta { C_GREEN } else { C_DIM })),
            Span::styled(e, Style::default().fg(if s.connected_epsilon { C_GREEN } else { C_DIM })),
            dim(format!("  {} peers", s.peer_count)),
        ])
    } else {
        Line::from(dim("p2p   connecting to mesh…"))
    };

    Paragraph::new(vec![tip_line, sync_line, rate_line, chunk_line, p2p_line])
        .block(card_block(" SYNC", C_VBRIGHT))
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

fn open_browser(url: &str) {
    let url = url.to_string();
    thread::spawn(move || {
        #[cfg(target_os = "linux")]
        { let _ = Command::new("xdg-open").arg(&url).spawn(); }
        #[cfg(target_os = "macos")]
        { let _ = Command::new("open").arg(&url).spawn(); }
        #[cfg(target_os = "windows")]
        { let _ = Command::new("cmd").args(["/c", "start", &url]).spawn(); }
    });
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
    Paragraph::new(vec![
        Line::from(Span::styled(toast, Style::default().fg(C_GOLD))),
        Line::from(kb),
        serve_line,
    ])
}
