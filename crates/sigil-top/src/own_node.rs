//! Your own node, inside sigil-top (10.6.0, Viktor 2026-10-20: "a single unified sigil graph binary for both so
//! users that download the node like bitcoin node" — "running sigil graph top node should just do it all").
//!
//! One program. `sigil-top` runs THE node — [`sigil_node::node_main`], the exact code the network's producer and
//! followers run, not a port of it — plus the screen, the wallet and mining:
//!
//!   * no terminal (systemd, docker, a pipe, or `SIGIL_HEADLESS=1`): the node runs in this process, exactly like
//!     `sigil-node start`. That is how the network's own nodes run it.
//!   * a terminal: sigil-top starts itself a second time as the node ([`CHILD_ENV`], output to a log file next to
//!     the node's data) and draws the screen. The node prints thousands of lines and stops the process on fatal
//!     errors; as a child it can do both without touching the screen, and the screen restarts it.
//!
//! A node configured by its environment (`SIGIL_DB_PATH` set — the producer's systemd unit, the follower
//! containers) runs exactly as configured; nothing here changes a variable. Otherwise sigil-top fills in what a
//! follower of the network it was built for needs ([`G3_PROFILE`] — the live g3 followers' chain settings,
//! measured 2026-10-20), a data folder beside the block store, and free ports. A user's node FOLLOWS: it
//! downloads every block, checks it and applies it; it never produces (`SIGIL_PRODUCER=0`).
//!
//! `SIGIL_TOP_NODE=0` turns the node off (the screen alone, as before 10.6.0).
//!
//! 🪤 The node has its own gossip auto-update (`TOPIC_RELEASE`), which swaps the file at `current_exe()` — under
//! sigil-top that file is sigil-top. It is OFF unless release keys are pinned (none are compiled in; nobody sets
//! `SIGIL_TRUSTED_RELEASE_KEYS`, measured 2026-10-20). sigil-top's signed-manifest updater is the only updater;
//! never pin node release keys for a node that runs inside sigil-top.
//!
//! What the screen shows of the node comes from the node's own local API — `/v1/integrity` (height, finalized
//! height), `/v1/network/topology` (peers), `/v1/finality/certificate` (the network's certified tip),
//! `/v1/dagknight/recent` (block hashes for the DATA INTEGRITY card) — written into [`status`].
//!
//! Before 10.6.0 sigil-top carried a SECOND node implementation (`producer/run.rs` + `sync.rs`: its own sync and
//! produce loop, without the finality-gate settings, the deposit door or the STATE DIVERGENCE refusal). It is gone.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Set on the copy of sigil-top that the screen starts as its node.
pub const CHILD_ENV: &str = "SIGIL_TOP_NODE_CHILD";
/// Where the node's local API answers (`http://127.0.0.1:<port>`), for the screen's own code.
pub const API_ENV: &str = "SIGIL_TOP_NODE_API";

/// The node runs unless `SIGIL_TOP_NODE=0`.
pub fn enabled() -> bool {
    !matches!(std::env::var("SIGIL_TOP_NODE").as_deref().map(str::trim), Ok("0") | Ok("off") | Ok("false") | Ok("no"))
}

/// This process is the node child of a screen.
pub fn is_child() -> bool {
    std::env::var(CHILD_ENV).as_deref() == Ok("1")
}

/// The environment already configures a node (an operator's unit or container) — run it exactly as given.
pub fn configured_by_env() -> bool {
    std::env::var("SIGIL_DB_PATH").map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// The g2 producer's hybrid ids (`hex[@from],…`): the key retired on 2026-09-15 and the rotated one.
pub const G2_TRUSTED_PRODUCER_IDS: &str =
    "ccb24eab7f4d975d9aac142837bcea8a4f8a559b6497c7fbe83b91118d78bb5b,0512706efc948876251f9c5010106dea11cf3cdc49d9193ce98a4d6e9f222fb4";
/// The g3 producer's hybrid id: the key made for the g3 launch (2026-10-19), blake3(sqisign_pk || ed25519_pk).
pub const G3_TRUSTED_PRODUCER_IDS: &str = "43effcd6eef0d42088b3c83c9acd7c45b51661abd8c7133cb4bf97b86207c745";
/// What a downloaded node trusts: the producer of the chain this binary was built for.
pub const DEFAULT_TRUSTED_PRODUCER_IDS: &str =
    if sigil_header::IS_G3 { G3_TRUSTED_PRODUCER_IDS } else { G2_TRUSTED_PRODUCER_IDS };

/// The chain settings every g3 follower runs with — copied from the live followers node-4..8 on 2026-10-20
/// (`docker inspect`), minus what is per node (identity, ports, paths, keys). These are what a node must agree
/// on to verify the same chain: the DAG rule, the finality committee and gate, the trusted producer, the
/// emission clock, the court bench. Filled in only where the environment has no value.
pub const G3_PROFILE: &[(&str, &str)] = &[
    ("SIGIL_PRODUCER", "0"),
    ("SIGIL_TRANSPORT", "direct"),
    ("SIGIL_DAG", "1"),
    ("SIGIL_DAG_GHOSTDAG_K", "4"),
    ("SIGIL_DAG_PENDING_MAX_TIP_LAG", "512"),
    ("SIGIL_FINALITY_GATE", "solo"),
    ("SIGIL_FINALITY_CHECKPOINT_INTERVAL", "1"),
    ("SIGIL_FINALITY_COMMITTEE", "6d517c7907c2c27982434251a188803a8782918dfc2d5c47ce6dbc1cf4b4749c"),
    ("SIGIL_TRUSTED_PRODUCER_ID_HEX", G3_TRUSTED_PRODUCER_IDS),
    ("SIGIL_GENESIS_TS", "1792418887"),
    ("SIGIL_EMISSION_ADAPTIVE", "1"),
    ("SIGIL_RATE_ADAPTIVE", "1"),
    ("SIGIL_RATE_MIN", "8"),
    ("SIGIL_RATE_MAX", "110"),
    ("SIGIL_RATE_TARGET_LATENCY_MS", "2000"),
    ("SIGIL_PRODUCE_US", "27000"),
    ("SIGIL_PRODUCE_GRACE_MS", "3000"),
    ("SIGIL_SHARE_EASE_BITS", "32"),
    ("SIGIL_PAY_SHARE_POOL", "1"),
    ("SIGIL_MINING_CREDIT_WINDOW", "100"),
    ("SIGIL_HYBRID_CHECKPOINT_INTERVAL", "32768"),
    ("SIGIL_HYBRID_ENFORCE", "0"),
    ("SIGIL_FEED_EVERY", "10"),
    ("SIGIL_SEARCH_INDEX", "0"),
    ("SIGIL_SERVE_HEADERS_CAP", "8192"),
    ("SIGIL_SERVE_HOT_TTL_MS", "30000"),
    ("SIGIL_SERVE_EXPENSIVE_THROTTLE_MS", "750"),
    ("SIGIL_SERVE_FULL_INFLIGHT", "8"),
    ("SIGIL_SERVE_FULL_PER_PEER", "4"),
    ("SIGIL_COURT_BENCH", "87ed473b028cff8aed5ce27dfe97eac8e560f5fbe54020f01ca8f5db7e369c6e:chief,c0beb1a79e31f5db568d3377b48c260c2de11292d3110cf3e0b1ef4c36080917:justice,a6ca843bd7187aac2e8ddbf51dad66718248782da521a7551c8deeb2421ea212:justice,81e5c73296bf8ee00af3af76f6bd9d844ba54dafa3b4d155f7e4cb234c816aa3:justice,095b0e1f7f5bb258fb11427c4ac036e3d9e4f10fa39d7f282aa42862dc2b3dd8:justice"),
    ("SIGIL_COURT_HONOURS", "87ed473b028cff8aed5ce27dfe97eac8e560f5fbe54020f01ca8f5db7e369c6e:elephant:Kept the ledger honest - found and closed two silent-commitment faults (a bench root that hashed to nothing, and a seal that covered an order's name but not its terms), and published the measurements that prove it."),
    ("SIGIL_COURT_PUBLIC_BASE", "https://sigilgraph.org"),
    ("SIGIL_BRIDGE_ADMIN_WALLET", ""),
    ("SIGIL_BRIDGE_RELAYER_WALLET", ""),
];

/// Where the node lives and answers.
#[derive(Clone, Debug)]
pub struct Plan {
    pub data_dir: PathBuf,
    /// `http://127.0.0.1:<port>`, or empty when the node serves no HTTP API.
    pub api: String,
    pub log: PathBuf,
    /// sigil-top filled in the settings (false = the environment configured the node).
    pub user_mode: bool,
}

/// The first port in `range` nothing listens on (`host` = the address the node will bind).
fn free_port(host: &str, range: std::ops::RangeInclusive<u16>) -> Option<u16> {
    range.into_iter().find(|p| std::net::TcpListener::bind((host, *p)).is_ok())
}

fn set_default(k: &str, v: &str) {
    if std::env::var_os(k).is_none() {
        std::env::set_var(k, v);
    }
}

/// The node's folder: beside the place the block store REALLY lives (Viktor's `~/sigil-top-blocks.db` is a link
/// into the big array; following it keeps a growing chain off a small system disk), named by network.
fn default_data_dir() -> PathBuf {
    let store = PathBuf::from(sigil_top_sync::sigil_top_db_path());
    let real = std::fs::canonicalize(&store).unwrap_or(store);
    let base = real.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    base.join(format!("sigil-top-node-{}", sigil_header::NETWORK_ID_STR))
}

/// Secrets the node would otherwise pick up from fixed paths on the machine — an OPERATOR's, if sigil-top runs
/// on a server that also hosts the network (Epsilon). A user's node holds none of them, so each points into the
/// node's own folder, at a file that does not exist:
///   * the tip-proof key — else `~/.flux-agent-key.json`, a user's Flux agent key;
///   * the bridge vault key + ledger — else `/root/.config/sigil/bridge-vault.{seed,notes.jsonl}` (measured
///     2026-10-20: a user-mode node on Epsilon logged "bridge vault loaded"; the ledger was not written);
///   * the peer-id file — else the shared `/tmp/sigil-node-g0.peerid` a producer on this machine publishes.
fn follower_secret_paths(data_dir: &Path) -> Vec<(&'static str, String)> {
    let p = |name: &str| data_dir.join(name).to_string_lossy().into_owned();
    vec![
        ("SIGIL_TIP_PROOF_KEY", p("no-tip-proof-key")),
        ("SIGIL_BRIDGE_VAULT_SEED", p("no-bridge-vault.seed")),
        ("SIGIL_BRIDGE_VAULT_LEDGER", p("no-bridge-vault-notes.jsonl")),
    ]
}

/// A sigil-node producer on this machine: it published a plausible peer id AND `127.0.0.1:9501` accepts (a 300 ms
/// probe, so a stale file never becomes a dead bootstrap entry). Same rule as flux-p2p's light-client side.
fn local_producer(peerid_file: &Path) -> Option<String> {
    let pid = std::fs::read_to_string(peerid_file).ok()?;
    let pid = pid.trim();
    if !pid.starts_with("12D3Koo") || pid.len() < 46 || !pid.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let addr: std::net::SocketAddr = "127.0.0.1:9501".parse().ok()?;
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    Some(format!("/ip4/127.0.0.1/tcp/9501/p2p/{pid}"))
}

/// `0.0.0.0:18181` → `http://127.0.0.1:18181` (the screen talks to its node over loopback).
fn api_url(bind: &str) -> String {
    let bind = bind.trim();
    if bind.is_empty() {
        return String::new();
    }
    let port = bind.rsplit(':').next().unwrap_or("");
    format!("http://127.0.0.1:{port}")
}

/// Work out the plan; in user mode, also write the follower settings into this process's environment (only
/// where unset), so the node — here or in a child — reads them like an operator's unit. Call before any thread
/// that reads the environment starts.
pub fn prepare() -> std::io::Result<Plan> {
    if configured_by_env() {
        let data_dir = PathBuf::from(std::env::var("SIGIL_DB_PATH").unwrap_or_default());
        let api = api_url(&std::env::var("SIGIL_MONEY_API").unwrap_or_default());
        if !api.is_empty() {
            std::env::set_var(API_ENV, &api);
        }
        return Ok(Plan { log: data_dir.join("sigil-top-node.log"), data_dir, api, user_mode: false });
    }
    let data_dir = default_data_dir();
    std::fs::create_dir_all(&data_dir)?;
    let d = data_dir.to_string_lossy().into_owned();
    set_default("SIGIL_DB_PATH", &d);
    set_default("SIGIL_SNAPSHOT_DIR", &d);
    set_default("SIGIL_EXPECT_NETWORK", sigil_header::NETWORK_ID_STR);
    for (k, v) in follower_secret_paths(&data_dir) {
        set_default(k, &v);
    }
    if sigil_header::IS_G3 {
        for (k, v) in G3_PROFILE {
            set_default(k, v);
        }
    } else {
        set_default("SIGIL_PRODUCER", "0");
        set_default("SIGIL_TRUSTED_PRODUCER_ID_HEX", DEFAULT_TRUSTED_PRODUCER_IDS);
    }
    // A producer on THIS machine (Viktor's Epsilon): the public bootstrap address does not answer from the same box
    // (hairpin/firewall on :9501, measured 2026-10-20: a fresh node there sat at 0 peers), so dial it over loopback
    // first, the way the light engine already does. Read the shared peer-id file BEFORE pointing ours elsewhere.
    if std::env::var_os("SIGIL_BOOTSTRAP_PEERS").is_none() {
        if let Some(local) = local_producer(&flux_p2p::sigil_peerid_path()) {
            let mut list = vec![local];
            list.extend(sigil_net::DEFAULT_BOOTSTRAP_PEERS.iter().map(|s| s.to_string()));
            std::env::set_var("SIGIL_BOOTSTRAP_PEERS", list.join(","));
        }
    }
    // Every node writes its peer id to that shared file unless told otherwise — a user's node would overwrite a
    // producer's on the same machine and send every screen there dialing the wrong id. Ours lives with our data.
    set_default("SIGIL_PEERID_FILE", &data_dir.join("peerid").to_string_lossy());
    if std::env::var_os("SIGIL_P2P_PORT").is_none() {
        let p = free_port("0.0.0.0", 9501..=9600).unwrap_or(9501);
        std::env::set_var("SIGIL_P2P_PORT", p.to_string());
        set_default("SIGIL_WG_LISTEN_ADDR", &format!("/ip4/0.0.0.0/tcp/{p}"));
    }
    if std::env::var_os("SIGIL_MONEY_API").is_none() {
        // Loopback only: a user's node API is for this machine's screen and wallet, not the internet. Never the
        // screen's own mining relay port (18186 by default, bound later): whichever of the two lost that race
        // would break — measured risk on Epsilon, where 18181–18185 are taken and 18186 would be "free".
        let relay = crate::mining_relay::port();
        let p = (18181..=18280u16)
            .find(|p| *p != relay && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
            .unwrap_or(18181);
        std::env::set_var("SIGIL_MONEY_API", format!("127.0.0.1:{p}"));
    }
    let api = api_url(&std::env::var("SIGIL_MONEY_API").unwrap_or_default());
    std::env::set_var(API_ENV, &api);
    Ok(Plan { log: data_dir.join("sigil-top-node.log"), data_dir, api, user_mode: true })
}

/// The node's own local API, once the screen started one (empty otherwise).
pub fn api() -> String {
    std::env::var(API_ENV).unwrap_or_default()
}

/// Run the node in THIS process (`sigil-node start`). Returns its exit code.
fn run_here() -> ExitCode {
    sigil_node::node_main(vec!["sigil-node".into(), "start".into()])
}

/// No terminal: the node in this process, like `sigil-node start` — for systemd units, containers, servers.
pub fn run_headless() -> ExitCode {
    // sigil-top's own panic hook writes only to its startup trace file; a node's panic must reach stderr (the
    // journal, `docker logs`) exactly as under sigil-node. Default hook back, plus the trace line.
    drop(std::panic::take_hook());
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::boot_trace(&format!("PANIC: {info}"));
        default_hook(info);
    }));
    match prepare() {
        Ok(p) => {
            eprintln!(
                "sigil-top {}: running the node ({}) — data {}, API {}",
                env!("CARGO_PKG_VERSION"),
                if p.user_mode { "follower settings filled in by sigil-top" } else { "configured by the environment" },
                p.data_dir.display(),
                if p.api.is_empty() { "off".to_string() } else { p.api.clone() },
            );
        }
        Err(e) => {
            eprintln!("sigil-top: cannot prepare the node's data folder: {e}");
            return ExitCode::from(1);
        }
    }
    run_here()
}

/// Ask THIS process's node to stop the way systemd stops a node (SIGTERM / Ctrl-Break): it writes its state
/// snapshot and exits.
fn request_own_stop() {
    #[cfg(unix)]
    unsafe {
        extern "C" { fn kill(pid: i32, sig: i32) -> i32; fn getpid() -> i32; }
        kill(getpid(), 15);
    }
    #[cfg(windows)]
    unsafe {
        extern "system" { fn GenerateConsoleCtrlEvent(ev: u32, group: u32) -> i32; fn GetCurrentProcessId() -> u32; }
        GenerateConsoleCtrlEvent(1, GetCurrentProcessId()); // CTRL_BREAK_EVENT to our own process group
    }
}

/// The node child of a screen: stop when the screen goes away (its end of our stdin closes — also when it
/// crashes or is killed), then run the node.
pub fn run_child() -> ExitCode {
    let _ = std::thread::Builder::new().name("own-node-parent-watch".into()).spawn(|| {
        let mut buf = [0u8; 64];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        eprintln!("⏹ sigil-top node: the screen closed — stopping (state snapshot first)");
        request_own_stop();
        // If the stop request was not delivered, do not linger after the screen is gone.
        std::thread::sleep(Duration::from_secs(40));
        std::process::exit(0);
    });
    run_here()
}

/// What the node is doing, for the screen. Written by the host thread from the node's local API, read per frame.
pub mod status {
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

    pub const IDLE: u8 = 0; // the node is off (SIGIL_TOP_NODE=0)
    pub const SYNCING: u8 = 1; // downloading + applying blocks toward the network's tip
    pub const LIVE: u8 = 2; // at the network's tip
    pub const STOPPED: u8 = 3; // the node process is not running — reason in `message()`

    static PHASE: AtomicU8 = AtomicU8::new(IDLE);
    static HEIGHT: AtomicU64 = AtomicU64::new(0);
    static NET_TIP: AtomicU64 = AtomicU64::new(0);
    static SETTLED: AtomicU64 = AtomicU64::new(0);
    static PEERS: AtomicU64 = AtomicU64::new(0);
    static RESTARTS: AtomicU64 = AtomicU64::new(0);
    static RETRY_AT: AtomicU64 = AtomicU64::new(0);
    static MESSAGE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    static LOG: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

    /// Our node's newest `(height, block hash)` pairs, for the DATA INTEGRITY card's exact-at-height comparison
    /// against the network (from the node's own `/v1/dagknight/recent`, the same route the card reads from Epsilon).
    static SETTLED_RING: std::sync::Mutex<std::collections::VecDeque<(u64, [u8; 32])>> =
        std::sync::Mutex::new(std::collections::VecDeque::new());
    const SETTLED_CAP: usize = 8192;

    /// Keep the pairs in ascending height order; a pair below the newest one is ignored.
    pub fn push_settled(height: u64, hash: [u8; 32]) {
        if let Ok(mut g) = SETTLED_RING.lock() {
            if g.back().map(|(h, _)| *h >= height).unwrap_or(false) { return; }
            g.push_back((height, hash));
            while g.len() > SETTLED_CAP { g.pop_front(); }
        }
    }
    /// Our block hash at exactly `height`, if we still hold it.
    pub fn settled_hash_at(height: u64) -> Option<[u8; 32]> {
        let g = SETTLED_RING.lock().ok()?;
        g.iter().rev().find(|(h, _)| *h == height).map(|(_, x)| *x)
    }
    /// Highest height in the ring (0 = nothing yet).
    pub fn settled_top() -> u64 { SETTLED_RING.lock().ok().and_then(|g| g.back().map(|(h, _)| *h)).unwrap_or(0) }

    pub fn set_phase(p: u8) { PHASE.store(p, Ordering::Relaxed); }
    pub fn phase() -> u8 { PHASE.load(Ordering::Relaxed) }
    /// The last block the node APPLIED.
    pub fn set_height(h: u64) { HEIGHT.store(h, Ordering::Relaxed); }
    pub fn height() -> u64 { HEIGHT.load(Ordering::Relaxed) }
    /// The node's finalized height (certificates on g3).
    pub fn set_settled(h: u64) { SETTLED.store(h, Ordering::Relaxed); }
    pub fn settled() -> u64 { SETTLED.load(Ordering::Relaxed) }
    /// The network's tip as best known (the certified tip the node hears, or the light engine's view).
    pub fn note_net_tip(h: u64) { NET_TIP.fetch_max(h, Ordering::Relaxed); }
    pub fn net_tip() -> u64 { NET_TIP.load(Ordering::Relaxed) }
    /// How far our applied chain trails the network (0 when unknown).
    pub fn lag() -> u64 { net_tip().saturating_sub(height()) }
    pub fn set_peers(n: u64) { PEERS.store(n, Ordering::Relaxed); }
    pub fn peers() -> u64 { PEERS.load(Ordering::Relaxed) }
    pub fn note_restart() { RESTARTS.fetch_add(1, Ordering::Relaxed); }
    pub fn restarts() -> u64 { RESTARTS.load(Ordering::Relaxed) }
    pub fn set_retry_at(unix: u64) { RETRY_AT.store(unix, Ordering::Relaxed); }
    pub fn retry_at() -> u64 { RETRY_AT.load(Ordering::Relaxed) }
    pub fn set_message(m: impl Into<String>) { if let Ok(mut g) = MESSAGE.lock() { *g = m.into(); } }
    pub fn message() -> String { MESSAGE.lock().map(|g| g.clone()).unwrap_or_default() }
    pub fn set_log(p: impl Into<String>) { if let Ok(mut g) = LOG.lock() { *g = p.into(); } }
    pub fn log() -> String { LOG.lock().map(|g| g.clone()).unwrap_or_default() }

    /// One plain line for the dashboard, or `None` while the node is off.
    pub fn line() -> Option<String> {
        let (h, t) = (height(), net_tip());
        match phase() {
            IDLE => None,
            SYNCING => Some(if t > h && t > 0 {
                format!("your node: syncing h={h} of ~{t} ({:.1}%) — every block checked and applied here · peers {}", 100.0 * h as f64 / t as f64, peers())
            } else {
                format!("your node: starting — h={h} · peers {}", peers())
            }),
            LIVE => Some(format!("your node: LIVE — h={h} · finalized h={} · peers {} · the same program the network runs", settled(), peers())),
            _ => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let retry = match retry_at() { 0 => String::new(), t => format!(" · restarting in {}s", t.saturating_sub(now)) };
                Some(format!("your node: stopped — {}{retry}", message()))
            }
        }
    }
}

/// Blocks behind the network's tip that still count as "at the tip" (a few seconds of blocks on g3).
const LIVE_SLACK: u64 = 600;

fn get_json(url: &str) -> Option<serde_json::Value> {
    let r = crate::HTTP.get(url).timeout(Duration::from_secs(3)).send().ok()?;
    if !r.status().is_success() { return None; }
    r.json().ok()
}

/// One look at the node's local API → [`status`].
fn poll(api: &str, with_hashes: bool) -> bool {
    let Some(v) = get_json(&format!("{api}/v1/integrity")) else { return false };
    let h = v.get("height").and_then(|x| x.as_u64()).or_else(|| v.get("applied_height").and_then(|x| x.as_u64())).unwrap_or(0);
    status::set_height(h);
    if let Some(f) = v.get("finalized_height").and_then(|x| x.as_u64()) { status::set_settled(f); }
    if let Some(t) = get_json(&format!("{api}/v1/network/topology")) {
        let d = t.get("data").unwrap_or(&t);
        if let Some(n) = d.get("peer_count").and_then(|x| x.as_u64()) { status::set_peers(n); }
    }
    if let Some(c) = get_json(&format!("{api}/v1/finality/certificate")) {
        if let Some(ch) = c.get("certificate").and_then(|x| x.get("height")).and_then(|x| x.as_u64()) { status::note_net_tip(ch); }
    }
    if with_hashes {
        if let Some(r) = get_json(&format!("{api}/v1/dagknight/recent")) {
            let d = r.get("data").unwrap_or(&r);
            if let Some(items) = d.as_array().or_else(|| d.get("blocks").and_then(|b| b.as_array())) {
                let mut pairs: Vec<(u64, [u8; 32])> = items.iter().filter_map(|b| {
                    let h = b.get("height")?.as_u64()?;
                    let x = hex::decode(b.get("hash")?.as_str()?).ok()?;
                    Some((h, <[u8; 32]>::try_from(x.as_slice()).ok()?))
                }).collect();
                pairs.sort_by_key(|p| p.0);
                for (h, x) in pairs { status::push_settled(h, x); }
            }
        }
    }
    status::set_phase(if status::net_tip() > 0 && h + LIVE_SLACK >= status::net_tip() { status::LIVE } else { status::SYNCING });
    true
}

/// The node child, run and watched for the screen. Dropping it stops the node politely.
pub struct NodeHost {
    child: Arc<Mutex<Option<Child>>>,
    quit: Arc<AtomicBool>,
    watcher: Option<std::thread::JoinHandle<()>>,
    pub plan: Plan,
}

fn exe() -> PathBuf {
    crate::INSTALL_EXE.get().cloned().filter(|p| p.exists())
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("sigil-top"))
}

/// The last non-empty line of the node's log (why it stopped, usually).
fn last_log_line(log: &Path) -> String {
    let Ok(f) = std::fs::File::open(log) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut r = std::io::BufReader::new(f);
    if len > 16_384 { let _ = std::io::Seek::seek(&mut r, std::io::SeekFrom::Start(len - 16_384)); }
    let mut last = String::new();
    for line in r.lines().map_while(Result::ok) {
        let t = line.trim();
        if !t.is_empty() { last = t.chars().take(160).collect(); }
    }
    last
}

fn spawn_child(plan: &Plan) -> std::io::Result<Child> {
    // Keep one old log beside the current one; a node writes a lot.
    if std::fs::metadata(&plan.log).map(|m| m.len() > 64 << 20).unwrap_or(false) {
        let _ = std::fs::rename(&plan.log, plan.log.with_extension("log.1"));
    }
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(&plan.log)?;
    let _ = writeln!(log, "\n──── sigil-top {} starts its node ────", env!("CARGO_PKG_VERSION"));
    let err = log.try_clone()?;
    let mut cmd = Command::new(exe());
    cmd.env(CHILD_ENV, "1")
        .current_dir(&plan.data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP: Ctrl-Break reaches the node alone
    }
    cmd.spawn()
}

/// Ask the node child to stop like systemd stops a node, wait for its snapshot, then make sure it is gone.
fn stop_child(mut c: Child, wait: Duration) {
    #[cfg(unix)]
    unsafe {
        extern "C" { fn kill(pid: i32, sig: i32) -> i32; }
        kill(c.id() as i32, 15);
    }
    #[cfg(windows)]
    unsafe {
        extern "system" { fn GenerateConsoleCtrlEvent(ev: u32, group: u32) -> i32; }
        GenerateConsoleCtrlEvent(1, c.id());
    }
    drop(c.stdin.take()); // and the stdin watch, should the signal not arrive
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        if let Ok(Some(_)) = c.try_wait() { return; }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = c.kill();
    let _ = c.wait();
}

/// For the screen: prepare the node, start it as a child, watch it (restart with a growing pause if it stops),
/// and keep [`status`] fresh. `None` when the node is off or its folder cannot be made.
pub fn start_for_screen() -> Option<NodeHost> {
    if !enabled() { return None; }
    let plan = match prepare() {
        Ok(p) => p,
        Err(e) => {
            status::set_phase(status::STOPPED);
            status::set_message(format!("cannot make the node's data folder: {e}"));
            return None;
        }
    };
    status::set_log(plan.log.display().to_string());
    crate::boot_trace(&format!("own node: data {} · API {} · log {} · {}", plan.data_dir.display(), plan.api,
        plan.log.display(), if plan.user_mode { "user settings" } else { "configured by env" }));
    let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let quit = Arc::new(AtomicBool::new(false));
    let (child2, quit2, plan2) = (child.clone(), quit.clone(), plan.clone());
    let watcher = std::thread::Builder::new().name("own-node".into()).spawn(move || {
        let mut failures: u32 = 0;
        let mut started = Instant::now();
        let mut last_hashes = Instant::now() - Duration::from_secs(60);
        while !quit2.load(Ordering::Relaxed) {
            // (re)start
            let running = child2.lock().map(|g| g.is_some()).unwrap_or(false);
            if !running {
                match spawn_child(&plan2) {
                    Ok(c) => {
                        if let Ok(mut g) = child2.lock() { *g = Some(c); }
                        status::set_phase(status::SYNCING);
                        status::set_retry_at(0);
                        started = Instant::now();
                    }
                    Err(e) => {
                        status::set_phase(status::STOPPED);
                        status::set_message(format!("could not start: {e}"));
                    }
                }
            }
            // exited?
            let exited = child2.lock().ok().and_then(|mut g| {
                let st = g.as_mut()?.try_wait().ok()??;
                g.take();
                Some(st)
            });
            if let Some(st) = exited {
                if quit2.load(Ordering::Relaxed) { break; }
                // A node that ran a while and then stopped is a fresh failure, not a crash loop.
                if started.elapsed() > Duration::from_secs(600) { failures = 0; }
                failures += 1;
                status::note_restart();
                let why = last_log_line(&plan2.log);
                status::set_phase(status::STOPPED);
                status::set_message(format!("exited ({st}){}", if why.is_empty() { String::new() } else { format!(": {why}") }));
                let pause = match failures { 1 => 5, 2 => 15, 3 => 60, _ => 300 };
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                status::set_retry_at(now + pause);
                crate::tlog!("[own-node] node exited ({st}) — restarting in {pause}s; last log line: {why}");
                for _ in 0..pause * 5 {
                    if quit2.load(Ordering::Relaxed) { return; }
                    std::thread::sleep(Duration::from_millis(200));
                }
                continue;
            }
            if !plan2.api.is_empty() {
                let with_hashes = last_hashes.elapsed() >= Duration::from_secs(5);
                if with_hashes { last_hashes = Instant::now(); }
                if !poll(&plan2.api, with_hashes) && status::phase() != status::STOPPED {
                    status::set_phase(status::SYNCING); // started, API not answering yet
                }
            }
            for _ in 0..10 {
                if quit2.load(Ordering::Relaxed) { return; }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }).ok();
    Some(NodeHost { child, quit, watcher, plan })
}

impl NodeHost {
    /// Stop the node politely (the node writes its state snapshot first), waiting up to `wait`.
    pub fn stop(&mut self, wait: Duration) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(w) = self.watcher.take() { let _ = w.join(); }
        let c = self.child.lock().ok().and_then(|mut g| g.take());
        if let Some(c) = c { stop_child(c, wait); }
        status::set_phase(status::IDLE);
    }
}

impl Drop for NodeHost {
    fn drop(&mut self) { self.stop(Duration::from_secs(30)); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trusted_ids_follow_the_network() {
        if sigil_header::IS_G3 {
            assert_eq!(DEFAULT_TRUSTED_PRODUCER_IDS, G3_TRUSTED_PRODUCER_IDS);
            assert!(!DEFAULT_TRUSTED_PRODUCER_IDS.contains("0512706e"));
        } else {
            assert_eq!(DEFAULT_TRUSTED_PRODUCER_IDS, G2_TRUSTED_PRODUCER_IDS);
        }
        for id in DEFAULT_TRUSTED_PRODUCER_IDS.split(',') {
            let hex = id.split('@').next().unwrap();
            assert_eq!(hex.len(), 64);
            assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    /// The profile is a follower's and names no per-node thing (identity, ports, paths, keys).
    #[test]
    fn the_g3_profile_is_a_followers() {
        let get = |k: &str| G3_PROFILE.iter().find(|(x, _)| *x == k).map(|(_, v)| *v);
        assert_eq!(get("SIGIL_PRODUCER"), Some("0"));
        assert_eq!(get("SIGIL_TRUSTED_PRODUCER_ID_HEX"), Some(G3_TRUSTED_PRODUCER_IDS));
        for (k, _) in G3_PROFILE {
            let per_node = k.contains("SEED") || k.ends_with("_PORT") || k.ends_with("_KEY") || matches!(*k,
                "SIGIL_DB_PATH" | "SIGIL_SNAPSHOT_DIR" | "SIGIL_NODE_ID" | "SIGIL_MONEY_API"
                | "SIGIL_WG_LISTEN_ADDR" | "SIGIL_BOOTSTRAP_PEERS" | "SIGIL_PRODUCER_WALLET");
            assert!(!per_node, "{k} is per node, not a chain setting");
        }
        let mut keys: Vec<_> = G3_PROFILE.iter().map(|(k, _)| *k).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), G3_PROFILE.len(), "a key twice");
    }

    /// A user's node never reaches for an operator's secret on the same machine.
    #[test]
    fn a_followers_secrets_point_into_its_own_folder() {
        let d = Path::new("/srv/users/alice/sigil-top-node-sigil-g3");
        let got = follower_secret_paths(d);
        for k in ["SIGIL_TIP_PROOF_KEY", "SIGIL_BRIDGE_VAULT_SEED", "SIGIL_BRIDGE_VAULT_LEDGER"] {
            let v = got.iter().find(|(x, _)| *x == k).map(|(_, v)| v.as_str()).unwrap_or_else(|| panic!("{k} missing"));
            assert!(v.starts_with("/srv/users/alice/sigil-top-node-sigil-g3/"), "{k} = {v}");
            assert!(!v.contains("/root/") && !v.contains(".flux-agent-key"), "{k} = {v}");
        }
    }

    #[test]
    fn a_garbage_peer_id_file_is_never_a_bootstrap_entry() {
        let d = std::env::temp_dir().join(format!("own-node-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("peerid");
        std::fs::write(&f, "12D3Koo; rm -rf /").unwrap();
        assert_eq!(local_producer(&f), None);
        assert_eq!(local_producer(&d.join("missing")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn api_url_is_loopback() {
        assert_eq!(api_url("0.0.0.0:18184"), "http://127.0.0.1:18184");
        assert_eq!(api_url("127.0.0.1:18209"), "http://127.0.0.1:18209");
        assert_eq!(api_url(""), "");
    }

    // The settled ring is one process-wide list; integrity_ui's test drives it (two tests on it would race).

    #[test]
    fn status_line_shapes() {
        status::set_phase(status::IDLE);
        assert!(status::line().is_none());
        status::set_phase(status::SYNCING);
        status::set_height(500);
        status::note_net_tip(1000);
        let l = status::line().unwrap();
        assert!(l.contains("h=500") && l.contains("50.0%"), "{l}");
        status::set_phase(status::STOPPED);
        status::set_message("exited (exit status: 1): genesis mismatch");
        assert!(status::line().unwrap().contains("genesis mismatch"));
        status::set_phase(status::IDLE);
    }
}
