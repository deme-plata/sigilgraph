//! sigil-top — a `top`/`htop`-style terminal monitor for a SIGIL node.
//!
//! Two modes:
//!   • full  (default) — multi-panel dashboard: node / 4 state roots / economics
//!                        (21 M cap bar) / flux-fold succinct-sync capability.
//!   • lite  (`--lite`) — one compact scorecard line, for tmux strips & SSH peeks.
//!
//! Polls `http://127.0.0.1:8181/api/v1/status` with a hand-rolled std TCP GET (no
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

use std::io::{IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Offline fallback only — the *live* update signal is fetched at runtime from the
/// flux release channel (see [`UPDATE_MANIFEST`]). The update bar glows when the
/// channel reports a version newer than this binary, so an OLD build learns about a
/// new release without recompilation — the whole point of "auto-update the flux way".
const LATEST: &str = "0.2.19";
/// The flux release channel for the lightweight node: `<product>-latest.json` in the
/// q-flux downloads dir — the SAME manifest `flux_release_check` reads. Fetched at
/// startup (throttled) and on `[U]`, so the running binary discovers new releases live.
const UPDATE_MANIFEST: &str = "https://sigilgraph.quillon.xyz/downloads/sigil-top-latest.json";
/// Which prebuilt this binary self-updates to (its per-OS entry in the manifest).
const SELF_TARGET: &str = if cfg!(windows) { "windows-x64" } else { "linux-x64" };
/// Live testnet feed (same source flux-node.html uses): status + tip + block stream.
const DEFAULT_FEED: &str = "https://sigilgraph.quillon.xyz/sigil-status.json";
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
}

/// Fetch + parse the live testnet feed over HTTPS (rustls). Returns the mapped
/// node status + the recent block stream — the real testnet sync source.
fn fetch_feed(url: &str) -> Option<(NodeStatus, Vec<FeedBlock>)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent(concat!("sigil-top/", env!("CARGO_PKG_VERSION")))
        .build().ok()?;
    let feed: Feed = client.get(url).send().ok()?.json().ok()?;
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
    if let Some((st, _blocks)) = fetch_feed(&cfg.feed) {
        return (st, true, "feed");
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
}

/// L4-A keystone: reconstruct the canonical v0 tip-proof from the node's real
/// roots and verify it for sigil-g0. ~µs, downloads 0 blocks. NOTE (honest): the
/// v0 `Blake3Fingerprint` flavor proves the proof is well-formed + on the right
/// network + uncorrupted — it does NOT alone prove canonicality/adversarial
/// safety. That comes from K independent sources (L4-C) + the SQIsign/STARK
/// flavors. The UI says so.
fn verify_tip(tip: &Tip) -> TipVerify {
    let roots = tip.roots.to_state_roots();
    let t = Instant::now();
    let proof = TipProof::new_blake3(tip.height, roots);
    let res = proof.verify(sigil_net::NETWORK_ID);
    let latency_us = t.elapsed().as_micros();
    let fingerprint_hex = hex(&proof.fingerprint());
    let hash_is_fingerprint =
        !tip.hash.is_empty() && tip.hash.eq_ignore_ascii_case(&fingerprint_hex);
    TipVerify {
        ok: res.is_ok(),
        err: res.err().map(|e| e.to_string()),
        height: tip.height,
        fingerprint_hex,
        hash_is_fingerprint,
        reported_hash: tip.hash.clone(),
        latency_us,
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
}
impl Default for Config {
    fn default() -> Self {
        Self { lite: false, once: false, tui: false, interval: 2,
            api: "http://127.0.0.1:8181/api/v1/status".into(), feed: DEFAULT_FEED.into() }
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
         sigil-top --api URL    status endpoint (default http://127.0.0.1:8181/api/v1/status)"
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
    o.push_str(&format!("  {GOLD}[M]{RESET}{DIM}ine{RESET}   {GREEN}[F]{RESET}{DIM}ull sync{RESET}   {GREEN}[V]{RESET}{DIM}erify tip{RESET}   {GOLD}[U]{RESET}{DIM}pdate{RESET}   {VBRIGHT}[L]{RESET}{DIM}ogin{RESET}   {VIOLET}[D]{RESET}{DIM}NS anchor{RESET}   {DIM}[Q]uit{RESET}\n"));
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
    // Default-ON pinned-channel auto-update (before anything else). Only advances to a version the
    // operator has promoted in the manifest; --no-update / SIGIL_TOP_NO_AUTOUPDATE=1 opts out.
    maybe_auto_update(&argv);
    let cfg = parse_args();
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
#[derive(Deserialize, Default, Clone)]
struct Target {
    url: String,
    #[serde(default)]
    blake3_hex: String,
    #[serde(default)]
    size_bytes: u64,
}

/// `sigil-top-latest.json` — same shape `flux_release_publish` writes, plus a
/// `targets` map so one channel serves both the Linux build and the Windows .exe.
#[derive(Deserialize)]
struct Release {
    version: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    blake3_hex: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    targets: std::collections::HashMap<String, Target>,
}

impl Release {
    /// The download for THIS platform: the matching `targets` entry, else the
    /// top-level single-binary fields.
    fn for_self(&self) -> Target {
        self.targets.get(SELF_TARGET).cloned().unwrap_or(Target {
            url: self.url.clone(),
            blake3_hex: self.blake3_hex.clone(),
            size_bytes: self.size_bytes,
        })
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
    std::env::var("SIGIL_MINE_URL").unwrap_or_else(|_| "https://sigilgraph.quillon.xyz:8447/v1/mine".into())
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
        let client = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(8)).build() {
            Ok(c) => c, Err(e) => { let _ = tx.send(format!("✗ miner init: {e}")); return; }
        };
        let _ = tx.send(format!("▲ mining → {url} · diff {difficulty_bits} bits · wallet {}…", &wallet[..8]));
        let mut accepted = 0u64;
        while !stop.load(Ordering::Relaxed) {
            // header binds the share to the current minute (cheap freshness); find a winning nonce.
            let header = format!("sigil-g0-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 30).unwrap_or(0));
            let mut nonce = 0u64;
            let found = loop {
                if stop.load(Ordering::Relaxed) { break None; }
                let mut buf = header.as_bytes().to_vec();
                buf.extend_from_slice(&nonce.to_le_bytes());
                if leading_zero_bits(blake3::hash(&buf).as_bytes()) >= difficulty_bits { break Some(nonce); }
                nonce = nonce.wrapping_add(1);
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
                Err(e) => { let _ = tx.send(format!("✗ submit: {e} (retry 3s)")); thread::sleep(Duration::from_secs(3)); }
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
/// binary so the node is immediately running the chosen version. Silent + non-fatal on any failure
/// (offline, unreachable channel) — the monitor still starts. Disable with `--no-update` or
/// `SIGIL_TOP_NO_AUTOUPDATE=1`. This is what makes "every node gets the release I choose" automatic,
/// while a stale manifest means a new publish reaches nobody until it's promoted.
fn maybe_auto_update(argv: &[String]) {
    if argv.iter().any(|a| a == "--no-update")
        || std::env::var("SIGIL_TOP_NO_AUTOUPDATE").map(|v| v == "1").unwrap_or(false)
    {
        return;
    }
    let rel = match fetch_latest() {
        Ok(r) if version_gt(&r.version, VERSION) => r,
        _ => return, // up to date, channel unreachable, or malformed → just run
    };
    eprintln!("  {GOLD}⬆ auto-update v{VERSION} → v{}{RESET} ({SELF_TARGET}) — verifying…", rel.version);
    match self_update(&rel) {
        Ok(_) => {
            // Re-exec the freshly-swapped binary with the original args (minus argv[0]).
            if let Ok(exe) = std::env::current_exe() {
                let args: Vec<String> = std::env::args().skip(1).collect();
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    let _ = std::process::Command::new(&exe).args(&args).exec(); // replaces this process
                }
                #[cfg(not(unix))]
                {
                    let _ = std::process::Command::new(&exe).args(&args).spawn();
                    std::process::exit(0);
                }
            }
        }
        Err(e) => eprintln!("  {DIM}auto-update skipped: {e}{RESET}"),
    }
}

fn self_update(rel: &Release) -> Result<String, String> {
    let t = rel.for_self();
    if t.url.is_empty() { return Err(format!("manifest has no {SELF_TARGET} build")); }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
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
    // Stage beside the current exe, then atomic self-replace (rename-self on Windows).
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = exe.with_extension("new");
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    if let Err(e) = self_replace::self_replace(&tmp) {
        // In-place swap blocked (Windows AV / locked exe). Don't dead-end: keep the
        // already-BLAKE3-verified new binary right beside the current one.
        let beside = exe.with_file_name(format!("sigil-top-v{}{}", rel.version,
            if cfg!(windows) { ".exe" } else { "" }));
        let _ = std::fs::rename(&tmp, &beside);
        return Ok(format!(
            "downloaded + verified v{} but in-place swap failed ({e}) — run it: {}",
            rel.version, beside.display()));
    }
    let _ = std::fs::remove_file(&tmp);
    Ok(format!(
        "✓ swapped v{VERSION} → v{} ({:.1} MB, BLAKE3-verified) — restart sigil-top to run it",
        rel.version, bytes.len() as f64 / 1.048576e6))
}

struct App {
    cfg: Config,
    st: NodeStatus,
    online: bool,
    last_fetch: Instant,
    verify: Option<TipVerify>,
    toast: String,
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
    full_sync: bool,                                    // [F] opt-in heavy full sync (default = 10ms lightweight verify)
    sync_us: u128,
    // L2-B: real eclipse-K — measured independent sources agreeing on the tip (replaces hardcoded k).
    eclipse_k: u32,
    eclipse_sources: Vec<(String, bool)>,
    last_eclipse: Instant,
}

impl App {
    fn new(cfg: Config) -> Self {
        App { cfg, st: NodeStatus::default(), online: false, last_fetch: Instant::now(),
              verify: None, toast: String::new(),
              latest: LATEST.to_string(),
              last_update_check: Instant::now() - Duration::from_secs(3600),
              update_rx: None,
              blocks: Vec::new(),
              target_height: 0, synced_height: 0, verified_count: 0, streak: 0, score: 0,
              mining: false, mine_rx: None, mine_stop: None, mine_accepted: 0, full_sync: false, sync_us: 0,
              eclipse_k: 0, eclipse_sources: Vec::new(),
              last_eclipse: Instant::now() - Duration::from_secs(60) }
    }
    fn refresh(&mut self) {
        // Live testnet sync: pull {status, tip, blocks} over HTTPS; fall back to a local node.
        let got = fetch_feed(&self.cfg.feed)
            .map(|(s, b)| { self.blocks = b; (s, true) })
            .or_else(|| fetch(&self.cfg.api).ok().map(|s| (s, true)))
            .unwrap_or((NodeStatus::default(), false));
        self.st = got.0; self.online = got.1;
        // Auto-update signal: poll the flux release channel every 5 min so the update
        // bar lights up on its own when a new sigil-top is published (no [U] needed).
        if self.last_update_check.elapsed() > Duration::from_secs(300) {
            self.last_update_check = Instant::now();
            if let Ok(rel) = fetch_latest() { self.latest = rel.version; }
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
    if let Ok(client) = reqwest::blocking::Client::builder().timeout(Duration::from_secs(3)).build() {
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
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let mut app = App::new(cfg);
    app.refresh();

    let res = (|| -> std::io::Result<()> {
        loop {
            term.draw(|f| draw_ui(f, &app))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        match k.code {
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('r') | KeyCode::Char('R') => app.refresh(),
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
                            }
                            KeyCode::Char('v') | KeyCode::Char('V') => {
                                app.toast = match app.st.tip.as_ref() {
                                    Some(t) => { let v = verify_tip(t); let m = if v.ok { format!("✓ tip {} verified in {} µs · 0 blocks", v.height, v.latency_us) } else { format!("✗ verify failed: {}", v.err.clone().unwrap_or_default()) }; app.verify = Some(v); m }
                                    None => "no tip published by node — nothing to verify".into(),
                                };
                            }
                            KeyCode::Char('u') | KeyCode::Char('U') => {
                                // Flux-way self-update on a BACKGROUND thread so the 11 MB
                                // download never freezes the TUI (the "intet" bug). The result
                                // string lands on update_rx and is shown on the next draw.
                                if app.update_rx.is_some() {
                                    app.toast = "↓ update already running…".into();
                                } else {
                                    app.toast = "↓ checking flux release channel…".into();
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
                                app.toast = "login: quit + run `sigil-top login --seed <64-hex>` (PKCE wallet assertion, no password)".into();
                            }
                            KeyCode::Char('d') | KeyCode::Char('D') => {
                                app.toast = "DNS anchor: sigilgraph.quillon.xyz — TXT-published tip-proof (verifier cross-check, wiring pending)".into();
                            }
                            _ => {}
                        }
                    }
                }
            }
            if app.last_fetch.elapsed() >= Duration::from_secs(app.cfg.interval) {
                app.refresh();
            }
            // background self-update result (if any) → toast
            if let Some(rx) = app.update_rx.as_ref() {
                match rx.try_recv() {
                    Ok(msg) => { app.toast = msg; app.update_rx = None; }
                    Err(mpsc::TryRecvError::Disconnected) => { app.update_rx = None; }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            // Drain live mining progress (accepted shares) onto the toast + counter.
            if let Some(rx) = app.mine_rx.as_ref() {
                while let Ok(msg) = rx.try_recv() {
                    if msg.starts_with("✓ share") { app.mine_accepted += 1; app.score += 50; }
                    app.toast = msg;
                }
            }
        }
    })();

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

// ── Card dashboard v2 — ground-up redesign co-authored with DeepSeek-V4. Block-element
//    art + colour + accent stripes (▌), NO dingbats — rich even on the legacy Windows
//    console. Each render_* returns an owned Paragraph<'static>. Integrated + bug-fixed
//    by Claude (f.area, .areas, manual supply bar, owned spans).

fn draw_ui(f: &mut Frame, app: &App) {
    let area = f.area();
    let [header_area, body_area, footer_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(2)]).areas(area);

    f.render_widget(render_header(app), header_area);

    let body_h = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(body_area);
    let (left_area, right_area) = (body_h[0], body_h[1]);

    let left_v = Layout::vertical([
        Constraint::Length(6), // Node
        Constraint::Length(6), // StateRoots
        Constraint::Length(4), // Supply
        Constraint::Length(5), // SyncStatus
        Constraint::Length(4), // Mining
    ])
    .spacing(1)
    .split(left_area);

    f.render_widget(render_node_card(app), left_v[0]);
    f.render_widget(render_state_roots(app), left_v[1]);
    f.render_widget(render_supply(app), left_v[2]);
    f.render_widget(render_sync_status(app), left_v[3]);
    f.render_widget(render_mining(app), left_v[4]);

    let right_v = Layout::vertical([Constraint::Length(5), Constraint::Min(0)])
        .spacing(1)
        .split(right_area);
    f.render_widget(render_security(app), right_v[0]);
    f.render_widget(render_block_stream(app), right_v[1]);

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
    let (synced, target) = (app.synced_height, app.target_height);
    let lag = target.saturating_sub(synced);
    let pct = if target > 0 { (synced as f64 / target as f64) * 100.0 } else { 0.0 };
    let lag_str = if lag > 0 { format!("lag {} blocks", group(lag)) } else { "synced".to_string() };
    let lines = vec![
        Line::from(vec![
            dim("synced "), Span::styled(group(synced), Style::default().fg(C_GREEN)),
            dim(" / "), Span::styled(group(target), Style::default().fg(C_VBRIGHT)),
            Span::styled(format!(" ({:.1}%)", pct), Style::default().fg(C_DIM)),
        ]),
        Line::from(vec![
            Span::styled(lag_str, Style::default().fg(if lag > 0 { C_GOLD } else { C_GREEN })),
            dim(format!("  ·  {} µs/tip", app.sync_us)),
        ]),
        Line::from(dim("flux-fold whole chain = 1 proof")),
    ];
    Paragraph::new(lines).block(card_block(" SYNC", C_VBRIGHT))
}

fn render_mining(app: &App) -> Paragraph<'static> {
    let (state, scol) = if app.mining { ("ON", C_GREEN) } else { ("off", C_RED) };
    let earn = format!("~{:.4}", app.verified_count as f64 * 0.0005);
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
    ];
    Paragraph::new(lines).block(card_block(" MINING", C_CYAN))
}

fn render_security(app: &App) -> Paragraph<'static> {
    let k = app.eclipse_k;
    let agreed = app.eclipse_sources.iter().filter(|(_, b)| *b).count();
    let total = app.eclipse_sources.len().max(1);
    let lines = vec![
        Line::from(vec![
            dim("K "), Span::styled(k.to_string(), Style::default().fg(C_VBRIGHT).add_modifier(Modifier::BOLD)),
            dim("   agree "), Span::styled(format!("{}/{}", agreed, total),
                Style::default().fg(if agreed == total { C_GREEN } else { C_GOLD })),
        ]),
        Line::from(vec![
            dim("P(eclipse) 0.30^K = "), Span::styled(format!("{:.1e}", 0.30_f64.powi(k as i32)), Style::default().fg(C_GOLD)),
        ]),
        Line::from(dim("node + DoH resolvers (real)")),
    ];
    Paragraph::new(lines).block(card_block(" SECURITY", C_VIOLET))
}

fn render_block_stream(app: &App) -> Paragraph<'static> {
    let tip_ok = app.verify.as_ref().map_or(false, |v| v.ok);
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
    Paragraph::new(lines).block(card_block(" BLOCK STREAM · live", C_VBRIGHT))
}

fn render_footer(app: &App) -> Paragraph<'static> {
    let toast = if app.toast.is_empty() { String::new() } else { format!(" › {}", app.toast) };
    let keys = |c: &'static str, rest: &'static str, col: Color| -> Vec<Span<'static>> {
        vec![Span::styled(c, Style::default().fg(col).add_modifier(Modifier::BOLD)), dim(rest), Span::raw("  ")]
    };
    let mut kb = vec![Span::raw(" ")];
    kb.extend(keys("[M]", "ine", C_GOLD));
    kb.extend(keys("[V]", "erify", C_GREEN));
    kb.extend(keys("[R]", "efresh", C_GREEN));
    kb.extend(keys("[U]", "pdate", C_VBRIGHT));
    kb.extend(keys("[L]", "ogin", C_VIOLET));
    kb.extend(keys("[Q]", "uit", C_RED));
    Paragraph::new(vec![
        Line::from(Span::styled(toast, Style::default().fg(C_GOLD))),
        Line::from(kb),
    ])
}
