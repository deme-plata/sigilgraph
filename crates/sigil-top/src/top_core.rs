//! The types and helpers of the sigil-top binary, split out of main.rs (2026-10-08) so
//! main.rs holds only main(). Many modules inline these small helpers; while they shared
//! main()'s codegen unit, any edit to main() re-optimized every module that inlined one.
use super::*;

pub(crate) static LAST_PANIC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn remember_panic(msg: String) {
    let mut slot = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(msg);
}

pub(crate) fn last_panic_line() -> String {
    LAST_PANIC
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| "[PANIC] <panic hook did not run>".into())
}

/// Which base the last successful manifest fetch used — binary downloads follow it.
pub(crate) static ACTIVE_BASE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Short display form of [`FLUX_REV`] (strip `full:`, first 10 chars).
pub(crate) fn short_rev() -> String {
    let r = FLUX_REV.strip_prefix("full:").unwrap_or(FLUX_REV);
    r.chars().take(10).collect()
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct NodeStatus {
    #[serde(alias = "network_id")]
    pub(crate) network: String,
    pub(crate) version: String,
    #[serde(alias = "block_height", alias = "tip_height")]
    pub(crate) height: u64,
    #[serde(alias = "peer_count", alias = "peers_connected")]
    pub(crate) peers: u64,
    #[serde(alias = "producer_tag")]
    pub(crate) producer: String,
    #[serde(alias = "uptime")]
    pub(crate) uptime_secs: u64,
    #[serde(alias = "supply", alias = "minted_supply")]
    pub(crate) native_supply: u128,
    #[serde(alias = "wallet_state_root")]
    pub(crate) wallet_root: String,
    #[serde(alias = "dex_state_root")]
    pub(crate) dex_root: String,
    #[serde(alias = "event_log_root")]
    pub(crate) event_root: String,
    #[serde(alias = "contract_state_root")]
    pub(crate) contract_root: String,
    // L4-A: the node publishes a real, verifiable tip — {height, hash, roots:{4×[u8;32]}}.
    // Present on the live sigilgraph-testnet snapshot; absent on bare local nodes
    // (which still report the top-level string aliases above, kept for back-compat).
    pub(crate) tip: Option<Tip>,
    #[serde(default)]
    pub(crate) blocks_per_sec: f64,
    /// v0.2.35: wallet balance for the logged-in miner (u128 raw, 8 decimals). Zero when
    /// the feed doesn't carry it yet — non-breaking, always present.
    #[serde(default, alias = "balance")]
    pub(crate) wallet_balance: u128,
    /// v7.1.13: the publisher's own freshness stamp (unix seconds). 0 when the
    /// feed doesn't carry it. Used to refuse zombie mirrors: a feed can answer
    /// HTTP 200 with WEEKS-old data (the dist-fluxapp copy froze 2026-07-27 and
    /// every client worldwide trusted tip 33,598,726 for 13 days).
    #[serde(default)]
    pub(crate) updated: u64,
    /// v0.42: false while the node is (re)building its explorer full-text index in
    /// the background — money/chain data is live, only search is briefly empty.
    /// Absent on older nodes → defaults true (don't show a stale "indexing" badge).
    #[serde(default = "default_true")]
    pub(crate) index_ready: bool,
    /// L-F 0.77.5: per-block reward in whole SIGIL (fractional after halvings,
    /// e.g. 5 → 2.5). The feed has carried reward_sig all along but it was
    /// dropped on the floor here — which is why "SIGIL/s" vanished from the UI
    /// when mining moved in-node (the v0.64.2 ONE-GRAPH cut pointed at a Node
    /// tab that never received the network stats). 0.0 = unknown (old feed).
    #[serde(default)]
    pub(crate) reward_sig: f64,
}

pub(crate) fn default_true() -> bool { true }

/// The real, per-block tip the node publishes (LIGHT-3 L3-A, live).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Tip {
    pub(crate) height: u64,
    /// Full block hash, hex. (May or may not equal the tip-proof fingerprint —
    /// the client computes the fingerprint and shows the truth, never assumes.)
    pub(crate) hash: String,
    pub(crate) roots: TipRoots,
}

/// The four committed state roots, as the node serializes them (byte arrays).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TipRoots {
    pub(crate) wallet_state_root: [u8; 32],
    pub(crate) dex_state_root: [u8; 32],
    pub(crate) event_log_root: [u8; 32],
    pub(crate) contract_state_root: [u8; 32],
}

impl TipRoots {
    pub(crate) fn to_state_roots(&self) -> StateRoots {
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
pub(crate) struct FeedBlock {
    pub(crate) height: u64,
    pub(crate) hash: String,
    pub(crate) producer: String,
    pub(crate) txs: u64,
    pub(crate) tip_ms: u64,
}

/// The `hash-at` report. A height this store does not hold is `null` / "—", never an empty hash.
pub(crate) fn format_hash_rows(synced_to: u64, rows: &[(u64, Option<String>)], json: bool) -> String {
    if json {
        let items: Vec<String> = rows.iter().map(|(h, x)| match x {
            Some(x) => format!("{{\"height\":{h},\"hash\":\"{x}\"}}"),
            None => format!("{{\"height\":{h},\"hash\":null}}"),
        }).collect();
        format!("{{\"synced_to\":{synced_to},\"blocks\":[{}]}}\n", items.join(","))
    } else {
        let mut o = format!("synced_to {synced_to}\n");
        for (h, x) in rows { o.push_str(&format!("{h} {}\n", x.as_deref().unwrap_or("—"))); }
        o
    }
}

/// Newest height first, one entry per height (a later duplicate of the same height is dropped), capped.
pub(crate) fn merge_block_stream(blocks: &mut Vec<FeedBlock>, cap: usize) {
    blocks.sort_by(|a, b| b.height.cmp(&a.height));
    blocks.dedup_by_key(|b| b.height);
    blocks.truncate(cap);
}

/// v0.7.0: A node in the AI operator's fleet. Tracked for uptime and version compliance.
#[derive(Debug, Clone)]
pub(crate) struct FleetNode {
    pub(crate) name: String,
    pub(crate) addr: String,
    pub(crate) port: u16,
    pub(crate) online: bool,
    pub(crate) height: u64,
    pub(crate) version: String,
    pub(crate) uptime_secs: u64,
}

/// The live testnet feed — {status, tip, blocks} — the same JSON flux-node.html
/// consumes. sigil-top syncs from this over HTTPS.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Feed {
    pub(crate) status: FeedStatus,
    pub(crate) tip: Option<Tip>,
    pub(crate) blocks: Vec<FeedBlock>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct FeedStatus {
    pub(crate) height: u64,
    pub(crate) peers: u64,
    pub(crate) agents: u64,
    pub(crate) supply: String, // "6,294,505" whole SIGIL, comma-grouped
    // Per-block reward — fractional after halvings (e.g. 5 → 2.5 → 1.25), so it
    // MUST be a float. Typing it u64 made serde reject the WHOLE feed (the bug
    // that silently forced the light node OFFLINE on every machine).
    pub(crate) reward_sig: f64,
    pub(crate) network_id: String,
    pub(crate) live: bool,
    #[serde(default)]
    pub(crate) blocks_per_sec: f64,
}

/// Fetch + parse the live testnet feed over HTTPS (rustls). Returns the mapped
/// node status + the recent block stream — the real testnet sync source.
pub(crate) static LAST_FEED_ERR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// 0.77: ONE shared pooled HTTP client for the per-tick fetchers (feed, block fallback,
/// eclipse-k). Building a fresh Client per call opened a new TCP+TLS connection per tick
/// that then sat in TIME_WAIT — over a multi-hour genesis-archive sync that exhausted
/// Windows' ephemeral ports (the "tip frozen / error sending request" bug, #156 item 3).
/// Timeouts are set PER REQUEST (each call site keeps its old value); the TLS posture is
/// the union of the old per-site builders (TLS_1_2 feeds + the invalid-cert fallback).
pub(crate) static HTTP: std::sync::LazyLock<reqwest::blocking::Client> = std::sync::LazyLock::new(|| {
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

pub(crate) struct Config {
    pub(crate) lite: bool,
    pub(crate) once: bool,
    /// Opt-in ratatui TUI (alt-screen, interactive keys). Default is the original
    /// hand-rolled obsidian/violet dashboard that people liked.
    pub(crate) tui: bool,
    pub(crate) interval: u64,
    pub(crate) api: String,
    /// Live testnet feed URL (HTTPS): {status, tip, blocks}. The TUI syncs from this.
    pub(crate) feed: String,
    /// Toast set by startup auto-update (shown in TUI footer, not stderr).
    pub(crate) initial_toast: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self { lite: false, once: false, tui: true, interval: 2,
            api: crate::help::DEFAULT_API.into(), feed: DEFAULT_FEED.into(),
            initial_toast: None }
    }
}

pub(crate) fn parse_args() -> Config {
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

pub(crate) fn print_help() {
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
pub(crate) fn one_chain_view(st: &mut NodeStatus, spine_off: bool) -> Option<u64> {
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

pub(crate) fn flux_home() -> String { std::env::var("HOME").unwrap_or_else(|_| "/root".into()) }

pub(crate) fn hex_to_32(h: &str) -> Option<[u8; 32]> {
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
    // len() is BYTES and the slice below is byte-indexed: a 64-BYTE string with a multibyte
    // char (e.g. "a€…") would pass the length check, then `&h[i*2..i*2+2]` could split a
    // UTF-8 boundary and PANIC. Valid hex is ASCII, so require that up front.
    if h.len() != 64 || !h.is_ascii() { return None; }
    let mut o = [0u8; 32];
    for i in 0..32 { o[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?; }
    Some(o)
}

/// `sigil-top login --seed <hex64>` — the wallet signs an OAuth2 PKCE auth
/// request to prove ownership (no password). On success we persist the public
/// wallet id (never the secret) and the dashboard greets you by wallet.
pub(crate) fn do_login(seed_hex: Option<String>) {
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

/// The install path captured at startup, BEFORE any self-replace. Afte
/// `self_replace` swaps the binary, `/proc/self/exe` (what `current_exe()` reads)
/// points at the moved-aside OLD inode — often a "(deleted)" path — so a relaunch
/// that spawns `current_exe()` fails with ENOENT even though the NEW binary sits
/// at this original path. Capturing it up front makes relaunch reliable.
pub(crate) static INSTALL_EXE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The [F] → full archive toast (the screen's own light engine; only when your node is off).
pub(crate) fn full_node_toast(base: &str) -> String {
    base.to_string()
}

/// The [F] → light toast.
pub(crate) fn light_monitor_toast() -> String {
    "◇ LIGHT MONITOR — verifies the live tip (10ms proof), holds nothing new. Press F for full archive.".to_string()
}

/// Your own node runs (10.6.0) — sigil-top always runs one unless SIGIL_TOP_NODE=0.
pub(crate) fn own_node_on() -> bool {
    #[cfg(feature = "producer")]
    {
        return crate::own_node::enabled();
    }
    #[allow(unreachable_code)]
    false
}

/// [F] while your own node runs: there is nothing to switch.
pub(crate) fn own_node_f_toast() -> String {
    "◆ Your node keeps and checks the whole chain itself — nothing to switch. (Before 10.6.0 F started a full node; now one always runs.)".to_string()
}

/// v0.7.0: Poll each fleet node's status API to check uptime, height, and version.
/// Runs on the UI thread (quick timeout per node — 3s each). AI operators depend on
/// this to know if their fleet needs attention.
pub(crate) fn check_fleet_health(nodes: &mut Vec<FleetNode>) {
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

pub(crate) fn version_gt(a: &str, b: &str) -> bool {
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
pub(crate) fn mine_url() -> String {
    std::env::var("SIGIL_MINE_URL").unwrap_or_else(|_| "https://sigilgraph.fluxapp.xyz:8447/v1/mine".into())
}
/// LANE-N: where the operator's chosen mining wallet is persisted. The [W] wallet
/// posts its (keyed) address here via `/api/v1/use-wallet` so mining credits the
/// wallet the operator actually sees AND holds the private key for.
pub(crate) fn mine_wallet_path() -> String { format!("{}/.flux/sigil-mine-wallet", flux_home()) }

/// A 64-hex lowercase/upper address (the node keys balances by this).
pub(crate) fn valid_addr(s: &str) -> bool {
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
pub(crate) fn resolve_mine_wallet(
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
    let has_any = seed_derived.is_some()
        || env.as_deref().map(valid_addr).unwrap_or(false)
        || chosen.as_deref().map(valid_addr).unwrap_or(false);
    // NEVER mine to a keyless address again (2026-10-02). The old fallback hashed the
    // hostname — and where HOSTNAME is unset (Windows, tmux, containers) EVERY such install
    // hashed the same word, so they all credited a406208d…e138 = blake3("sigil-top-miner:
    // sigil-top"): 84,213.66 SIGIL measured that day, in an address nobody holds a key for.
    // With no wallet configured, create a real one once (seed → 0600 file, reloaded by
    // `miner_seed`), and mine to that.
    if !has_any {
        static CREATED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        if let Some(addr) = CREATED.get_or_init(|| match ai_tools::create_wallet() {
            Ok((addr, path)) => { tlog!("[wallet] no wallet configured — created {}… (seed in {path}, owner-only)", &addr[..12]); Some(addr) }
            Err(e) => { tlog!("[wallet] ⚠ could not create a wallet ({e}) — mining has no keyed address"); None }
        }) { return addr.clone(); }
    }
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
    // 2026-10-02: also the wallet this install created for itself (0600 file), so a
    // restart keeps the SAME keyed wallet instead of falling back to a keyless address.
    let seed_hex = std::env::var("SIGIL_MINE_SEED").ok()
        .or_else(|| std::fs::read_to_string(ai_tools::wallet_seed_path()).ok())?;
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

/// Start REAL mining on a background thread: find a BLAKE3 nonce meeting `difficulty_bits`, POST it to
/// the node's `/mine` endpoint, repeat. Accepted shares are reported over the returned channel so the
/// TUI shows live progress. Stops when `stop` flips true. This is what makes pressing **[M]** actually
/// mine — not just toggle a flag. Light difficulty (testnet) so shares land in seconds.
pub(crate) fn start_mining(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> mpsc::Receiver<String> {
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
        // 2026-10-15: the seed the miner signs with — SIGIL_MINE_SEED OR the wallet file this
        // install made/imported (`miner_seed`). Reading only the env var meant every wallet
        // created by sigil-top itself mined but never published its receiving key, so nobody
        // could pay it (a 189 SIGIL phone payment bounced on exactly such a wallet).
        #[cfg(feature = "shield-register")]
        if let Some(seed_hex) = miner_seed().map(hex::encode) {
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
pub(crate) fn leading_zero_bits(d: &[u8]) -> u32 {
    let mut n = 0u32;
    for &b in d { if b == 0 { n += 8; } else { n += b.leading_zeros(); break; } }
    n
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
    // 10.6.0: your own node FOLLOWS the chain, so shares credit at the network's producer; the local producer
    // mining endpoint that the second node implementation served (2026-08-25) is gone with it.
    // 2026-10-02: a node that has verified the chain (10 ms tip proof or full sync) serves
    // its own mining endpoint and balances shares across upstream nodes — so a rig never
    // depends on one URL. See `mining_relay.rs` for what this does and does not decentralise.
    if mining_relay::relay_is_up() {
        return mining_relay::url();
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








/// Whether the light monitor joins the p2p mesh at launch. Default ON; `SIGIL_SPINE_SYNC=0`
/// (or empty) or an explicit no-sync flag turns it off; `SIGIL_SPINE_SYNC=1` forces it on.
pub(crate) fn mesh_join_default(spine_env: Option<&str>, no_sync_flag: bool) -> bool {
    match spine_env {
        Some(v) => !(v == "0" || v.is_empty()),
        None => !no_sync_flag,
    }
}
