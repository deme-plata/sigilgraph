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

/// The node runs unless `SIGIL_TOP_NODE=0` — or, with no such setting, unless light mode was chosen with [F] (10.6.5).
pub fn enabled() -> bool {
    match env_choice() {
        Some(on) => on,
        None => !chosen_light(),
    }
}

/// `SIGIL_TOP_NODE` in the environment, if set: true = node on.
fn env_choice() -> Option<bool> {
    std::env::var("SIGIL_TOP_NODE").ok().map(|v| !matches!(v.trim(), "0" | "off" | "false" | "no"))
}

/// 10.6.5 (Viktor 2026-10-23: "some users are going to use sigil just as miner not full archive"): the remembered
/// choice, beside the node's folder — "light" (no node: mine and watch the network with the quick tip check) or "full"
/// (the default).
fn mode_file() -> PathBuf {
    default_data_dir().with_file_name(format!("sigil-top-node-{}.mode", sigil_header::NETWORK_ID_STR))
}

fn chosen_light() -> bool {
    std::fs::read_to_string(mode_file()).map(|s| s.trim() == "light").unwrap_or(false)
}

fn write_mode(mode: &str) {
    let p = mode_file();
    let tmp = p.with_extension("mode.tmp");
    if std::fs::write(&tmp, mode).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// The screen's node, where [F] can reach it.
static HOST: std::sync::Mutex<Option<NodeHost>> = std::sync::Mutex::new(None);

/// Start the screen's node (when the mode wants one) and keep it where [F] can reach it.
pub fn screen_start() {
    if let Some(h) = start_for_screen() {
        if let Ok(mut g) = HOST.lock() {
            *g = Some(h);
        }
    }
}

/// The running node's log, if one runs (for the exit message).
pub fn screen_log() -> Option<String> {
    HOST.lock().ok().and_then(|g| g.as_ref().map(|n| n.plan.log.display().to_string()))
}

/// Before an update hands over to the new version (2026-10-23): stop this screen's node first, so the new
/// version's node finds its port and data folder free. Says so, because it can take up to 30 s.
pub fn stop_before_update() {
    let running = HOST.lock().map(|g| g.is_some()).unwrap_or(false);
    if running {
        println!("  stopping your node before the update (it writes its state first, up to 30 s)…");
        screen_stop_at_exit();
    }
}

/// At exit: stop the node politely (state snapshot first).
pub fn screen_stop_at_exit() {
    let h = HOST.lock().ok().and_then(|mut g| g.take());
    if let Some(mut n) = h {
        n.stop(Duration::from_secs(30));
    }
}

/// [F]: switch between your full node and light mode, remember the choice, and say what changed.
pub fn toggle_light_mode() -> String {
    if env_choice().is_some() {
        return "your node is set by SIGIL_TOP_NODE in this program's environment — remove it to switch with [F]".into();
    }
    let running = HOST.lock().map(|g| g.is_some()).unwrap_or(false);
    if running {
        write_mode("light");
        // 10.5.0 remembered "full archive" for the old light engine; light mode must not wake it.
        crate::startup_util::persist_sync_mode("light");
        let h = HOST.lock().ok().and_then(|mut g| g.take());
        if let Some(mut n) = h {
            // 2026-10-23: tell the watcher to quit FIRST, then flip the screen now — the stop itself can take up to 30 s
            // (Windows), and until it finished the screen kept showing the node, so [F] looked ignored.
            n.quit.store(true, Ordering::Relaxed);
            status::set_phase(status::IDLE);
            if let Ok(j) = std::thread::Builder::new().name("own-node-stop".into()).spawn(move || n.stop(Duration::from_secs(30))) {
                if let Ok(mut g) = STOPPING.lock() { *g = Some(j); }
            }
        }
        "◇ LIGHT MODE — your node stops (its data is kept). sigil-top mines and watches the network with its quick tip check, holding no chain. [F] again = full node".into()
    } else {
        write_mode("full");
        // 2026-10-23: a stop still running (pressed [F] twice quickly) must finish before a new node starts — two nodes
        // on one folder made the new one attach to the dying old one, and the screen never changed.
        let pending = STOPPING.lock().ok().and_then(|mut g| g.take());
        match pending {
            Some(j) if !j.is_finished() => {
                let _ = std::thread::Builder::new().name("own-node-restart".into()).spawn(move || {
                    let _ = j.join();
                    screen_start();
                });
                "◆ FULL NODE — your node starts as soon as the old one has finished stopping (a few seconds); it resumes where it stopped. [F] again = light mode".into()
            }
            _ => {
                screen_start();
                "◆ FULL NODE — your node starts and checks every block itself; it resumes where it stopped. [F] again = light mode".into()
            }
        }
    }
}

/// The stop [F] started for light mode, while it runs (2026-10-23).
static STOPPING: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

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
        note_filled(k);
    }
}

/// Set unconditionally, and remember that user mode set it.
fn fill(k: &str, v: &str) {
    std::env::set_var(k, v);
    note_filled(k);
}

/// 10.6.3: the settings user mode filled in, listed in [`FILLED_ENV`] so a process STARTED BY this one can tell them
/// from an operator's configuration. The self-update relaunch inherits the environment: measured 2026-10-22 on
/// Viktor's PC, 10.6.2 ran with 10.6.0's `\\?\` SIGIL_DB_PATH, took the node for operator-configured and skipped
/// its own defaults — the per-install identity among them.
const FILLED_ENV: &str = "SIGIL_TOP_FILLED";
static FILLED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
fn note_filled(k: &str) {
    if let Ok(mut g) = FILLED.lock() {
        if !g.iter().any(|x| x == k) {
            g.push(k.to_string());
        }
    }
}

/// What 10.6.0–10.6.2 filled in user mode (they left no list): recognised by sigil-top's internal API_ENV plus a data
/// folder with sigil-top's own name.
const LEGACY_FILL_KEYS: &[&str] = &[
    "SIGIL_DB_PATH", "SIGIL_SNAPSHOT_DIR", "SIGIL_EXPECT_NETWORK", "SIGIL_TIP_PROOF_KEY", "SIGIL_BRIDGE_VAULT_SEED",
    "SIGIL_BRIDGE_VAULT_LEDGER", "SIGIL_PRODUCER", "SIGIL_TRUSTED_PRODUCER_ID_HEX", "SIGIL_BOOTSTRAP_PEERS",
    "SIGIL_PEERID_FILE", "SIGIL_P2P_PORT", "SIGIL_WG_LISTEN_ADDR", "SIGIL_MONEY_API", "SIGIL_NODE_ID", API_ENV,
];

/// Clear what an earlier sigil-top filled in, so this one fills in afresh. Never touches anything else: only keys
/// that sigil-top itself lists (or, for 10.6.0–10.6.2, its own key set, recognised as above). The node child never
/// calls `prepare`, so it keeps exactly its parent's settings.
fn forget_inherited_fill() {
    let listed = std::env::var(FILLED_ENV).ok();
    let legacy = listed.is_none()
        && std::env::var_os(API_ENV).is_some()
        && std::env::var("SIGIL_DB_PATH").ok().is_some_and(|p| {
            Path::new(&p).file_name().is_some_and(|n| n.to_string_lossy().starts_with("sigil-top-node-"))
        });
    let keys: Vec<String> = match &listed {
        Some(l) => l.split(',').map(str::to_string).collect(),
        None if legacy => LEGACY_FILL_KEYS.iter().chain(G3_PROFILE.iter().map(|(k, _)| k)).map(|k| k.to_string()).collect(),
        None => return,
    };
    for k in keys.iter().filter(|k| k.starts_with("SIGIL_")) {
        // 10.6.0–10.6.2 set the bootstrap list only to put a local producer first; anything else is the user's.
        if legacy && k == "SIGIL_BOOTSTRAP_PEERS"
            && !std::env::var(k).is_ok_and(|v| v.starts_with("/ip4/127.0.0.1/"))
        {
            continue;
        }
        std::env::remove_var(k);
    }
    std::env::remove_var(FILLED_ENV);
    crate::boot_trace("own node: settings an earlier sigil-top filled in (self-update relaunch) were cleared — filling afresh");
}

/// The node's folder: beside the place the block store REALLY lives (Viktor's `~/sigil-top-blocks.db` is a link
/// into the big array; following it keeps a growing chain off a small system disk), named by network.
fn default_data_dir() -> PathBuf {
    let store = PathBuf::from(sigil_top_sync::sigil_top_db_path());
    let real = std::fs::canonicalize(&store).map(|p| plain_path(&p)).unwrap_or(store);
    let base = real.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    base.join(format!("sigil-top-node-{}", sigil_header::NETWORK_ID_STR))
}

/// `canonicalize` on Windows answers in the verbatim form `\\?\C:\Users\…` (10.6.0 showed it on the node card, and
/// it reaches the node as SIGIL_DB_PATH). Drop the prefix for a plain drive path; a `\\?\UNC\` share stays as it is.
fn plain_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with(r"UNC\") && rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
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
    forget_inherited_fill();
    if configured_by_env() {
        let data_dir = PathBuf::from(std::env::var("SIGIL_DB_PATH").unwrap_or_default());
        let api = api_url(&std::env::var("SIGIL_MONEY_API").unwrap_or_default());
        if !api.is_empty() {
            std::env::set_var(API_ENV, &api);
        }
        if !data_dir.as_os_str().is_empty() {
            own_keys(&data_dir);
        }
        return Ok(Plan { log: data_dir.join("sigil-top-node.log"), data_dir, api, user_mode: false });
    }
    let data_dir = default_data_dir();
    std::fs::create_dir_all(&data_dir)?;
    own_keys(&data_dir);
    if std::env::var_os("SIGIL_NODE_ID").is_none() {
        match node_identity(&data_dir) {
            Ok(id) => fill("SIGIL_NODE_ID", &id),
            Err(e) => crate::boot_trace(&format!("own node: no identity file ({e}) — the node falls back to its default name")),
        }
    }
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
            fill("SIGIL_BOOTSTRAP_PEERS", &list.join(","));
        }
    }
    // Every node writes its peer id to that shared file unless told otherwise — a user's node would overwrite a
    // producer's on the same machine and send every screen there dialing the wrong id. Ours lives with our data.
    set_default("SIGIL_PEERID_FILE", &data_dir.join("peerid").to_string_lossy());
    if std::env::var_os("SIGIL_P2P_PORT").is_none() {
        let p = free_port("0.0.0.0", 9501..=9600).unwrap_or(9501);
        fill("SIGIL_P2P_PORT", &p.to_string());
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
        fill("SIGIL_MONEY_API", &format!("127.0.0.1:{p}"));
    }
    let api = api_url(&std::env::var("SIGIL_MONEY_API").unwrap_or_default());
    fill(API_ENV, &api);
    if let Ok(g) = FILLED.lock() {
        std::env::set_var(FILLED_ENV, g.join(","));
    }
    Ok(Plan { log: data_dir.join("sigil-top-node.log"), data_dir, api, user_mode: true })
}

/// The node's own local API, once the screen started one (empty otherwise).
/// How far behind the network's tip your node may be for the wallet to read from it: 64 blocks ≈ 8 s on g3. Stricter
/// than the node line's "LIVE" (600): a balance must not be a minute old.
pub const WALLET_SLACK: u64 = 64;

/// 2026-10-23 (Viktor: "make the wallet use my own node"): your node's local API when the wallet may read from it —
/// it runs, it is caught up (≤ [`WALLET_SLACK`] behind the tip it has seen certified), and it answers locally.
pub fn caught_up_api() -> Option<String> {
    // the address the screen last heard back from (attached windows read another window's node), not the planned one
    let api = status::polled_api();
    wallet_may_read(&api, status::phase(), status::height(), status::net_tip()).then_some(api)
}

/// The rule behind [`caught_up_api`], pure so it is pinned by a test.
pub fn wallet_may_read(api: &str, phase: u8, height: u64, net_tip: u64) -> bool {
    !api.is_empty() && phase == status::LIVE && net_tip > 0 && height + WALLET_SLACK >= net_tip
}

pub fn api() -> String {
    std::env::var(API_ENV).unwrap_or_default()
}

/// The node exits with this when another node already holds its data folder (BSD EX_TEMPFAIL).
pub const EXIT_FOLDER_IN_USE: u8 = 75;
/// A headless node stopped to update, but the new binary could not be started in place: exit so the service manager
/// starts the program again (the new binary is in place, or the old one was put back).
pub const EXIT_UPDATE_RESTART: u8 = 76;
const LOCK_FILE: &str = "sigil-top-node.lock";
const API_FILE: &str = "sigil-top-node.api";

/// This install's network identity, in the node's own folder.
const NODE_ID_FILE: &str = "node-id";
const NODE_ID_PREFIX: &str = "sigil-top-";

/// 10.6.1: the node names itself `sigil-<net>-<HOSTNAME>` and takes its network identity (PeerId) from that
/// name. Windows has no HOSTNAME and most shells do not export it, so EVERY user
/// node was `sigil-sigil-g3-node` — one PeerId for all (measured 2026-10-22: Viktor's PC and a test node on
/// Epsilon were both 12D3KooWN1KRR…, and the second sat idle, never hearing the network's tip). Now one random
/// identity per install, written once, owner-only, reused on every start.
fn node_identity(dir: &Path) -> std::io::Result<String> {
    let p = dir.join(NODE_ID_FILE);
    if let Ok(s) = std::fs::read_to_string(&p) {
        let s = s.trim();
        let hex_part = s.strip_prefix(NODE_ID_PREFIX).unwrap_or("");
        if hex_part.len() == 64 && hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(s.to_string());
        }
    }
    let id = format!("{NODE_ID_PREFIX}{}", hex::encode(sigil_oauth::random_seed()));
    let tmp = p.with_extension("tmp");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(id.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &p)?;
    Ok(id)
}

/// One node per data folder. Neither the node nor its store locks anything, and two programs appending to one
/// chain.log damage it — two sigil-top windows, or a self-update whose new node starts while the old one is still
/// writing its shutdown snapshot. The node process holds an exclusive lock on `<data>/sigil-top-node.lock` for its
/// whole life; the OS releases it when the process ends, crash included. `Err` = another node holds it. A
/// filesystem that cannot lock at all only warns: the guard must never be the reason a node does not start.
fn lock_data_dir(dir: &Path) -> Result<Option<std::fs::File>, String> {
    let f = match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK_FILE)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("⚠ sigil-top node: cannot open {} ({e}) — running without the one-node-per-folder guard", dir.join(LOCK_FILE).display());
            return Ok(None);
        }
    };
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!("another node already runs on {}", dir.display())),
        Err(std::fs::TryLockError::Error(e)) => {
            eprintln!("⚠ sigil-top node: cannot lock {} ({e}) — running without the one-node-per-folder guard", dir.display());
            Ok(None)
        }
    }
}

/// Run the node in THIS process (`sigil-node start`), holding its folder. Returns its exit code.
fn run_here() -> ExitCode {
    let dir = PathBuf::from(std::env::var("SIGIL_DB_PATH").unwrap_or_else(|_| ".".into()));
    let _lock = match lock_data_dir(&dir) {
        Ok(l) => l,
        Err(why) => {
            eprintln!("⏸ sigil-top node: {why} — not starting a second one (nothing written)");
            return ExitCode::from(EXIT_FOLDER_IN_USE);
        }
    };
    // Where this node answers, for a second sigil-top window on the same folder (it shows this node).
    let api = api_url(&std::env::var("SIGIL_MONEY_API").unwrap_or_default());
    if !api.is_empty() {
        let _ = std::fs::write(dir.join(API_FILE), &api);
    }
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
            // 2026-10-23: a server node updates itself too (the screen's updater gate never runs without a screen).
            let exe = crate::INSTALL_EXE.get().cloned().or_else(|| std::env::current_exe().ok()).unwrap_or_default();
            let blocker = if std::env::args().any(|a| a == "--no-update") {
                Some("--no-update")
            } else {
                update_blocker(p.user_mode, |k| std::env::var(k).ok(), &exe, argv0_is_symlink())
            };
            match blocker {
                None => {
                    eprintln!("sigil-top: checks the signed release channel itself — first in ~10 min, then every ~6 h");
                    spawn_headless_updater();
                }
                Some(why) => eprintln!("sigil-top: no automatic updates on this node — {why}"),
            }
        }
        Err(e) => {
            eprintln!("sigil-top: cannot prepare the node's data folder: {e}");
            return ExitCode::from(1);
        }
    }
    let code = run_here();
    if let Some(v) = UPDATE_PENDING.get() {
        // relaunch_new_binary pre-flights again, then replaces this process (unix exec) or starts the new one and
        // exits (windows). It only RETURNS on failure; the swapped binary (or the put-back old one) is then what the
        // service manager starts next.
        eprintln!("sigil-top: node stopped for the update — restarting as v{v}");
        if !crate::self_update::relaunch_new_binary(v) {
            eprintln!("sigil-top: could not start v{v} in place — exiting so the service manager starts the program again");
            return ExitCode::from(EXIT_UPDATE_RESTART);
        }
    }
    code
}

// ── headless auto-update (2026-10-23) ────────────────────────────────────────────────────────────────────────────

/// The release the headless updater swapped in and pre-flighted; set just before it asks the node to stop.
static UPDATE_PENDING: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Why THIS headless node must not update itself, or `None` when it may. Pure, so the rules are pinned by tests.
/// A node configured by its environment (Epsilon's block-maker and its copies) is deployed by its operator,
/// follower-first, never by itself; a process holding a block-maker key never swaps its own binary; a binary started
/// through a symlink or living in a build folder (`target/`) is someone's deploy path, not an install.
fn update_blocker(
    user_mode: bool,
    env: impl Fn(&str) -> Option<String>,
    exe: &Path,
    started_via_symlink: bool,
) -> Option<&'static str> {
    let set = |k: &str| env(k).map(|v| !v.trim().is_empty()).unwrap_or(false);
    if env("SIGIL_TOP_NO_AUTOUPDATE").as_deref() == Some("1") {
        return Some("SIGIL_TOP_NO_AUTOUPDATE=1");
    }
    if !user_mode {
        return Some("it is configured by its environment, so its operator deploys it");
    }
    if set("SIGIL_PRODUCER_SQISIGN_SK_HEX") || set("SIGIL_FINALITY_SIGN_SEED") || env("SIGIL_PRODUCER").as_deref() == Some("1") {
        return Some("this process holds a block-maker key");
    }
    if started_via_symlink {
        return Some("the program was started through a symlink");
    }
    if exe.as_os_str().is_empty() {
        return Some("its install path is unknown");
    }
    if exe.components().any(|c| c.as_os_str() == "target") {
        return Some("the program lives in a build folder (target/)");
    }
    None
}

/// Was this program started through a symlink? `argv[0]` as typed — a path, or a name found on PATH. (The running
/// exe path the system reports is already resolved, so it cannot answer this.)
fn argv0_is_symlink() -> bool {
    let Some(a0) = std::env::args_os().next() else { return false };
    let p = PathBuf::from(&a0);
    let found = if p.components().count() > 1 {
        Some(p)
    } else {
        std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join(&a0)).find(|c| c.is_file()))
    };
    found.and_then(|f| std::fs::symlink_metadata(f).ok()).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

/// Check the signed channel in the background: ~10 min after start, then every ~6 h (each with a little jitter so
/// servers do not all ask at once; `SIGIL_TOP_UPDATE_EVERY_SECS` sets both, for a test). Stops after it hands over.
fn spawn_headless_updater() {
    std::thread::spawn(|| {
        let every = std::env::var("SIGIL_TOP_UPDATE_EVERY_SECS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|s| *s > 0);
        let jitter = |max: u64| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0) % max
        };
        std::thread::sleep(Duration::from_secs(every.unwrap_or(600 + jitter(300))));
        loop {
            match headless_update_once() {
                Ok(Some(v)) => {
                    let _ = UPDATE_PENDING.set(v);
                    request_own_stop();
                    return;
                }
                Ok(None) => {}
                Err(e) => eprintln!("sigil-top: update check: {e}"),
            }
            std::thread::sleep(Duration::from_secs(every.unwrap_or(6 * 3600 + jitter(1800))));
        }
    });
}

/// One check. `Ok(Some(v))` = v is swapped in at the install path and starts; `Ok(None)` = already current.
fn headless_update_once() -> Result<Option<String>, String> {
    let rel = crate::fetch_latest()?;
    if !crate::version_gt(&rel.version, crate::VERSION) {
        return Ok(None);
    }
    eprintln!("sigil-top: release v{} is out (running v{}) — downloading and checking it", rel.version, crate::VERSION);
    let msg = crate::self_update::self_update(&rel)?;
    if !msg.starts_with("swapped") {
        return Err(format!("v{} was downloaded but not swapped into place ({msg}) — keeping v{}", rel.version, crate::VERSION));
    }
    let exe = crate::INSTALL_EXE.get().cloned().or_else(|| std::env::current_exe().ok()).ok_or("no install path")?;
    match crate::preflight_binary(&exe) {
        Ok(rep) if rep.is_empty() || rep == rel.version || crate::version_gt(&rep, crate::VERSION) => {
            eprintln!("sigil-top: v{} is in place and starts ({msg}) — stopping the node (state first) to restart into it", rel.version);
            Ok(Some(rel.version))
        }
        Ok(rep) => {
            crate::self_update::revert_to_rollback(&exe);
            Err(format!("the new binary reports v{rep}, not v{} — the old one is back, keeping v{}", rel.version, crate::VERSION))
        }
        Err(e) => {
            crate::self_update::revert_to_rollback(&exe);
            Err(format!("the new binary does not start ({e}) — the old one is back, keeping v{}", crate::VERSION))
        }
    }
}

/// 2026-10-23 (Viktor: "nodes should automatically produce the key when startup"): this node's own block-making
/// identity, made on first start (`sigil_node::producer_signing::ensure_own_keys` — an identity, not a permission).
fn own_keys(dir: &Path) {
    if !dir.join(sigil_node::producer_signing::OWN_ID_FILE).exists() {
        eprintln!("sigil-top: making this node's own block-making keys (first start only, a few seconds)…");
    }
    match sigil_node::producer_signing::ensure_own_keys(dir) {
        Ok(Some(k)) => {
            let id = hex::encode(k.id);
            eprintln!(
                "sigil-top: this node's block-making identity {id} ({}{})",
                if k.created { "made now" } else { "kept" },
                if k.in_use { ", in use — this node makes blocks" } else { "" }
            );
            status::set_identity(id);
        }
        Ok(None) => {} // the operator's own keys
        Err(e) => eprintln!("sigil-top: could not make this node's block-making keys: {e}"),
    }
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
    static PID: AtomicU64 = AtomicU64::new(0);
    static RESYNC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// [Y] (10.6.3): ask the host to re-sync the node from block 0.
    pub fn request_resync() { RESYNC.store(true, Ordering::Relaxed); }
    pub fn take_resync() -> bool { RESYNC.swap(false, Ordering::Relaxed) }
    static MESSAGE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    static LOG: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    /// 2026-10-23: the node's last `/v1/sync/queue` answer (its history downloader), for the Queues tab.
    static QUEUE: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);
    pub fn set_queue(v: Option<serde_json::Value>) { if let Ok(mut g) = QUEUE.lock() { *g = v; } }
    pub fn queue() -> Option<serde_json::Value> { QUEUE.lock().ok().and_then(|g| g.clone()) }
    /// 2026-10-23: this node's public block-making identity (64 hex), once made.
    static IDENTITY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    pub fn set_identity(id: impl Into<String>) { if let Ok(mut g) = IDENTITY.lock() { *g = id.into(); } }
    pub fn identity() -> String { IDENTITY.lock().map(|g| g.clone()).unwrap_or_default() }
    /// 2026-10-23: the node API the screen last heard back from — its own node, or the node of another sigil-top
    /// window it attached to. The wallet reads from THIS address (one proven to answer), never from a planned one.
    static POLLED_API: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    pub fn set_polled_api(a: &str) { if let Ok(mut g) = POLLED_API.lock() { if *g != a { *g = a.to_string(); } } }
    pub fn polled_api() -> String { POLLED_API.lock().map(|g| g.clone()).unwrap_or_default() }
    fn id_suffix() -> String {
        let id = identity();
        if id.len() >= 12 { format!(" · block-making id {}…", &id[..12]) } else { String::new() }
    }

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
    /// The node process's id (0 = none running) — the SYNC card shows it, as 10.5.0 showed its own.
    pub fn set_pid(p: u64) { PID.store(p, Ordering::Relaxed); }
    pub fn pid() -> u64 { PID.load(Ordering::Relaxed) }
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
            LIVE => Some(format!("your node: LIVE — h={h} · finalized h={} · peers {} · the same program the network runs{}", settled(), peers(), id_suffix())),
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
    status::set_polled_api(api);
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
    // 2026-10-23: its download queue, for tab 4 (a node older than 10.7.1 has no such route → None).
    status::set_queue(get_json(&format!("{api}/v1/sync/queue")).filter(|q| q.get("in_flight").is_some()));
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
        // CREATE_NEW_PROCESS_GROUP: Ctrl-Break reaches the node alone. NORMAL_PRIORITY_CLASS: the screen lowers
        // itself to below-normal (platform.rs, so the miner never makes the PC sluggish) and a child would inherit
        // that, and fight the miner as an equal. At normal priority the node gets the cores first and the miner the
        // rest. (10.6.0's first sync on Viktor's PC ran at 217 blk/s vs ~800 on Epsilon; the cause is not measured,
        // a second sigil-top was running beside it — this removes one candidate.)
        cmd.creation_flags(0x0000_0200 | 0x0000_0020);
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

/// [Y] (10.6.3): the node's chain data — `aether/` (block log, index, saved state, snapshot keys) — moves to
/// `aether.old-<unix time>`; `mining-history` comes back into the fresh `aether/`. Kept, not deleted: the copy from an
/// EARLIER re-sync is removed first, so repeated re-syncs cannot fill the disk. The identity (`node-id`, keys,
/// `peerid`) and the user store `aether-user` stay where they are. The node must be stopped.
fn move_chain_aside(dir: &Path) -> std::io::Result<PathBuf> {
    let live = dir.join("aether");
    if !live.is_dir() {
        return Ok(live);
    }
    for e in std::fs::read_dir(dir)?.flatten() {
        if e.file_name().to_string_lossy().starts_with("aether.old-") && e.path().is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let old = dir.join(format!("aether.old-{secs}"));
    // On Windows a scanner can hold the folder a moment after the node exits: a few tries.
    let mut tries = 0;
    loop {
        match std::fs::rename(&live, &old) {
            Ok(()) => break,
            Err(e) if tries >= 10 => return Err(e),
            Err(_) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    std::fs::create_dir_all(&live)?;
    let mh = old.join("mining-history");
    if mh.is_dir() {
        let _ = std::fs::rename(&mh, live.join("mining-history"));
    }
    Ok(old)
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
        // Another sigil-top window's node holds the folder: show that node (its API), try our own again later.
        let mut attached: Option<String> = None;
        let mut spawn_after = Instant::now();
        let own_log = plan2.log.display().to_string();
        while !quit2.load(Ordering::Relaxed) {
            // [Y] (10.6.3, Viktor: "Y resync doesnt work"): stop the node politely, move its chain data aside,
            // start again from block 0.
            if status::take_resync() {
                if attached.is_some() {
                    crate::tlog!("[own-node] re-sync asked, but this node runs from another sigil-top window");
                } else {
                    let c = child2.lock().ok().and_then(|mut g| g.take());
                    if let Some(c) = c {
                        stop_child(c, Duration::from_secs(60));
                    }
                    status::set_pid(0);
                    match move_chain_aside(&plan2.data_dir) {
                        Ok(old) => crate::tlog!("[own-node] re-sync from block 0 — the previous chain data is kept in {}", old.display()),
                        Err(e) => crate::tlog!("[own-node] re-sync: could not move the chain data aside ({e}) — restarting on what it has"),
                    }
                    status::set_height(0);
                    status::set_settled(0);
                    status::set_phase(status::SYNCING);
                    failures = 0;
                    spawn_after = Instant::now();
                }
            }
            // (re)start
            let running = child2.lock().map(|g| g.is_some()).unwrap_or(false);
            if !running && Instant::now() >= spawn_after {
                match spawn_child(&plan2) {
                    Ok(c) => {
                        status::set_pid(c.id() as u64);
                        if let Ok(mut g) = child2.lock() { *g = Some(c); }
                        if attached.is_none() { status::set_phase(status::SYNCING); }
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
                status::set_pid(0);
                Some(st)
            });
            if let Some(st) = exited {
                if quit2.load(Ordering::Relaxed) { break; }
                if st.code() == Some(EXIT_FOLDER_IN_USE as i32) {
                    // Not a failure: our node already runs, from another sigil-top window (or an old copy is still
                    // writing its shutdown snapshot after an update). Show it; try to take over in a minute.
                    attached = std::fs::read_to_string(plan2.data_dir.join(API_FILE)).ok()
                        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                    status::set_log(format!("{own_log} · this node runs from another sigil-top window"));
                    spawn_after = Instant::now() + Duration::from_secs(if attached.is_some() { 60 } else { 5 });
                    continue;
                }
                attached = None;
                status::set_log(own_log.clone());
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
            let api = attached.clone().unwrap_or_else(|| plan2.api.clone());
            if !api.is_empty() {
                let with_hashes = last_hashes.elapsed() >= Duration::from_secs(5);
                if with_hashes { last_hashes = Instant::now(); }
                let ok = poll(&api, with_hashes);
                if !ok && attached.is_some() {
                    // The other window's node went away: start ours now.
                    attached = None;
                    status::set_log(own_log.clone());
                    spawn_after = Instant::now();
                } else if !ok && status::phase() != status::STOPPED {
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

    /// One identity per install: made once, read back the same, different in another folder, a damaged file replaced.
    #[test]
    fn each_install_has_its_own_lasting_identity() {
        let base = std::env::temp_dir().join(format!("own-node-id-test-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let first = node_identity(&a).unwrap();
        assert!(first.starts_with(NODE_ID_PREFIX) && first.len() == NODE_ID_PREFIX.len() + 64, "{first}");
        assert_eq!(node_identity(&a).unwrap(), first, "the same folder keeps its identity");
        assert_ne!(node_identity(&b).unwrap(), first, "another install gets another identity");
        std::fs::write(a.join(NODE_ID_FILE), "sigil-sigil-g3-node").unwrap();
        let fresh = node_identity(&a).unwrap();
        assert!(fresh != "sigil-sigil-g3-node" && fresh.starts_with(NODE_ID_PREFIX), "a damaged file is replaced");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(a.join(NODE_ID_FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the identity seeds the node's key — owner-only");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// [Y]: the chain data moves aside (one old copy kept), mining history and identity stay.
    #[test]
    fn resync_moves_the_chain_aside_and_keeps_identity_and_mining_history() {
        let d = std::env::temp_dir().join(format!("own-node-resync-test-{}", std::process::id()));
        std::fs::create_dir_all(d.join("aether/mining-history")).unwrap();
        std::fs::create_dir_all(d.join("aether-user")).unwrap();
        std::fs::write(d.join("aether/chain.log"), b"blocks").unwrap();
        std::fs::write(d.join("aether/mining-history/won"), b"7").unwrap();
        std::fs::write(d.join("node-id"), b"sigil-top-x").unwrap();
        let old = move_chain_aside(&d).unwrap();
        assert_eq!(std::fs::read(old.join("chain.log")).unwrap(), b"blocks", "the chain data is kept aside");
        assert!(!d.join("aether/chain.log").exists(), "the node starts from block 0");
        assert_eq!(std::fs::read(d.join("aether/mining-history/won")).unwrap(), b"7", "mining history stays");
        assert!(d.join("node-id").exists() && d.join("aether-user").is_dir(), "identity and user store untouched");
        std::fs::write(d.join("aether/chain.log"), b"new").unwrap();
        std::thread::sleep(Duration::from_millis(1100)); // a different second → a different folder name
        let old2 = move_chain_aside(&d).unwrap();
        let olds: Vec<_> = std::fs::read_dir(&d).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("aether.old-")).collect();
        assert_eq!(olds.len(), 1, "only the newest old copy is kept");
        assert_eq!(std::fs::read(old2.join("chain.log")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Windows' verbatim prefix is dropped for drive paths only.
    #[test]
    fn verbatim_prefix_is_dropped_for_drive_paths_only() {
        let got = plain_path(Path::new(r"\\?\C:\Users\Viktor S. Kristensen\AppData\Local\sigil-top-blocks.db"));
        assert_eq!(got, PathBuf::from(r"C:\Users\Viktor S. Kristensen\AppData\Local\sigil-top-blocks.db"));
        let unc = Path::new(r"\\?\UNC\server\share\x");
        assert_eq!(plain_path(unc), unc.to_path_buf());
        let unix = Path::new("/home/storage/sigil-top-blocks.db");
        assert_eq!(plain_path(unix), unix.to_path_buf());
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

    /// Two nodes on one folder would both append to one chain.log. The second is refused; the folder is free
    /// again once the first is gone (the OS drops the lock with the process, crash included).
    #[test]
    fn one_node_per_folder() {
        let d = std::env::temp_dir().join(format!("own-node-lock-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let first = lock_data_dir(&d).unwrap();
        assert!(first.is_some(), "the first node takes the folder");
        assert!(lock_data_dir(&d).is_err(), "a second node on the same folder must be refused");
        drop(first);
        assert!(lock_data_dir(&d).unwrap().is_some(), "free again once the first node is gone");
        let _ = std::fs::remove_dir_all(&d);
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

/// 2026-10-23: the wallet reads from your node only when it runs, answers locally and is at most 64 blocks behind.
#[cfg(test)]
mod wallet_reads_tests {
    use super::{status, wallet_may_read, WALLET_SLACK};

    #[test]
    fn the_wallet_reads_from_your_node_only_when_it_is_caught_up() {
        let api = "http://127.0.0.1:18182";
        assert!(wallet_may_read(api, status::LIVE, 1_000_000, 1_000_000));
        assert!(wallet_may_read(api, status::LIVE, 1_000_000 - WALLET_SLACK, 1_000_000), "64 behind: still yes");
        assert!(!wallet_may_read(api, status::LIVE, 1_000_000 - WALLET_SLACK - 1, 1_000_000), "65 behind: the network");
        assert!(!wallet_may_read(api, status::SYNCING, 1_000_000, 1_000_000), "still catching up");
        assert!(!wallet_may_read(api, status::STOPPED, 1_000_000, 1_000_000), "your node stopped");
        assert!(!wallet_may_read(api, status::IDLE, 1_000_000, 1_000_000), "light mode: no node of yours");
        assert!(!wallet_may_read("", status::LIVE, 1_000_000, 1_000_000), "no local API");
        assert!(!wallet_may_read(api, status::LIVE, 5, 0), "no certified tip seen yet");
    }
}

/// 2026-10-23: who may update itself without a screen. A user's server node may; Epsilon's block-maker and copies
/// (configured by their environment), a process with a block-maker key, a symlinked or build-folder binary may not.
#[cfg(test)]
mod headless_update_tests {
    use super::update_blocker;
    use std::path::Path;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn only_a_users_own_install_updates_itself() {
        let install = Path::new("/opt/sigil/sigil-top");
        // a user's node: the settings sigil-top fills in itself (SIGIL_PRODUCER=0, a placeholder tip-proof path)
        let user = [("SIGIL_PRODUCER", "0"), ("SIGIL_TIP_PROOF_KEY", "/var/lib/sigil/x/no-tip-proof-key")];
        assert_eq!(update_blocker(true, env(&user), install, false), None);
        // the operator's nodes: configured by their environment
        assert!(update_blocker(false, env(&[]), install, false).unwrap().contains("operator"));
        // block-maker keys, even in user mode
        for k in [("SIGIL_PRODUCER_SQISIGN_SK_HEX", "ab"), ("SIGIL_FINALITY_SIGN_SEED", "/k"), ("SIGIL_PRODUCER", "1")] {
            assert!(update_blocker(true, env(&[k]), install, false).unwrap().contains("block-maker"), "{k:?}");
        }
        // deploy paths, not installs
        assert!(update_blocker(true, env(&user), install, true).unwrap().contains("symlink"));
        let built = Path::new("/home/storage/sigil-g3-node-target/release/sigil-top");
        assert_eq!(update_blocker(true, env(&user), built, false), None, "a folder merely NAMED like a target is fine");
        let cargo = Path::new("/home/u/sigil/target/release/sigil-top");
        assert!(update_blocker(true, env(&user), cargo, false).unwrap().contains("build folder"));
        assert!(update_blocker(true, env(&user), Path::new(""), false).unwrap().contains("unknown"));
        // the opt-out
        assert!(update_blocker(true, env(&[("SIGIL_TOP_NO_AUTOUPDATE", "1")]), install, false).is_some());
    }
}
