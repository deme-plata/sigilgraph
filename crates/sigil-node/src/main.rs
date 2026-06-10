//! sigil-node — SIGIL block producer + verifier binary.
//!
//! Phase 0 wires together Track C (header + state + events) into a runnable
//! binary that proves the type composition works end-to-end. No networking,
//! no consensus, no real crypto: those crates land in P1+. The point of P0
//! is to be able to say `sigil-node mint-genesis` on Delta or Epsilon and
//! get a well-formed block 0 with all four roots computed locally.

mod block;
mod chain;
mod chain_log;
mod cli;
mod snapshot;

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use sigil_events::SigilEvent;
use sigil_header::{
    BlockHash, ProofBundle, SigScheme, SigilBlockHeaderV0, SignatureBytes, SqiSignature,
    StarkProof, WesolowskiProof, HEADER_VERSION, NETWORK_ID, SQISIGN_L5_LEN,
};
use sigil_state::{SigilState, StateMutation, StateRoots, StateTransition};
use sigil_tx::{apply_tx, ed25519_keygen, ed25519_sign_tx, Mempool, SignedTx, SigilTx};
use std::sync::{Arc, Mutex};

use crate::block::Block;
use crate::chain::ChainTip;
use crate::cli::Cli;

/// Point-to-point backfill request, sent over the flux-p2p request-response
/// channel (NOT gossipsub). The serving node answers a single requester with a
/// `BackfillResp` — no flood re-broadcast. Wire format is shared with the
/// sigil-top client; do not change these shapes.
#[derive(serde::Serialize, serde::Deserialize)]
struct BackfillReq {
    from: u64,
    to: u64,
    /// v0.7.27: the monitor (sigil-top) only stores HEADERS, so it asks for
    /// headers-only — the node then replies with a bincode `Vec<SigilBlockHeaderV0>`
    /// (≈20× smaller than full-block JSON, no JSON lexing) under the `H` magic.
    /// Old nodes don't have this field → serde defaults it false → full-block JSON
    /// (backward compatible). Node-to-node backfill leaves it false (needs full blocks).
    #[serde(default)]
    headers_only: bool,
    /// v0.33 (1M-blk/s lane): requested response codec for the headers_only path.
    /// 0 = raw `'H'+bincode` (default — old clients omit the field), 1 = `'Z'+zstd-1`.
    /// Measured on a real 4096-header chunk: 14.0× smaller (1019 → ~73 B/header) at
    /// ~20 ms compress — the wire stops being the sync bottleneck. Old servers ignore
    /// this field (serde_json skips unknown keys) and reply 'H'; clients decode both.
    #[serde(default)]
    codec: u8,
}

/// Point-to-point backfill response: the requested block range serialized as
/// JSON values (each element = `serde_json::to_value(&Block)`).
#[derive(serde::Serialize, serde::Deserialize)]
struct BackfillResp {
    blocks: Vec<serde_json::Value>,
}

const SCHEMA_VERSION: u16 = HEADER_VERSION;

/// Genesis-pinned trusted release-author SQIsign pubkeys (hex). The auto-updater
/// will ONLY apply a binary whose announcement is signed by one of these keys.
/// This is the single defense against fleet-wide RCE over the `/sigil/g0/release`
/// gossip topic — a release's authorizing key must be pinned here, NEVER read
/// from the announcement itself.
///
/// ⚠️ EMPTY BY DEFAULT = auto-update is OFF (fails closed). Populate with the
/// real release key(s) — run `sigil-updater keygen`, then paste the pubkey hex
/// here (or set `SIGIL_TRUSTED_RELEASE_KEYS`, comma-separated hex) — before
/// relying on OTA upgrades.
const TRUSTED_RELEASE_KEYS_HEX: &[&str] = &[
    // "<release-author SQIsign L5 pubkey hex>",
];

/// Build the trusted-release-key allowlist from the compiled-in constant plus
/// the optional `SIGIL_TRUSTED_RELEASE_KEYS` env (comma-separated hex). Invalid
/// hex entries are skipped with a warning rather than crashing the node.
fn trusted_release_keys() -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut push_hex = |h: &str| {
        let h = h.trim();
        if h.is_empty() {
            return;
        }
        match hex::decode(h) {
            Ok(b) => keys.push(b),
            Err(e) => eprintln!("⚠ SIGIL_TRUSTED_RELEASE_KEYS: skipping invalid hex '{}': {}", h, e),
        }
    };
    for k in TRUSTED_RELEASE_KEYS_HEX {
        push_hex(k);
    }
    if let Ok(env_keys) = std::env::var("SIGIL_TRUSTED_RELEASE_KEYS") {
        for k in env_keys.split(',') {
            push_hex(k);
        }
    }
    keys
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // v0.36.1 snapshot subcommands, dispatched here ahead of Cli::parse —
    // cli.rs is owned by a parallel work lane this cycle, so the two new
    // verbs live in main.rs (fold into the Cli enum at the next cli.rs touch).
    match args.get(1).map(|s| s.as_str()) {
        Some("snapshot-create") => {
            return match run_snapshot_create() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => { eprintln!("sigil-node: {:#}", e); ExitCode::from(1) }
            };
        }
        Some("snapshot-info") => {
            return match run_snapshot_info() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => { eprintln!("sigil-node: {:#}", e); ExitCode::from(1) }
            };
        }
        _ => {}
    }

    let cmd = Cli::parse(&args);

    let rc = match cmd {
        Cli::Help        => { print!("{}", Cli::usage()); return ExitCode::from(64); }
        Cli::Version     => { println!("sigil-node {} (header schema v{})",
                                       env!("CARGO_PKG_VERSION"), SCHEMA_VERSION); Ok(()) }
        Cli::Start       => run_start(),
        Cli::ShowTip     => run_show_tip(),
        Cli::MintGenesis => run_mint_genesis(),
        Cli::ProduceBlock { tx_file, broadcast, dry_run } => run_produce_block(&tx_file, broadcast, dry_run),
        Cli::WgUp { iface }   => run_wg_up(&iface),
        Cli::WgDown { iface } => run_wg_down(&iface),
        Cli::WgAddPeer { iface, public_key, endpoint, allowed_ips } =>
            run_wg_add_peer(&iface, &public_key, &endpoint, &allowed_ips),
        Cli::WgListPeers { iface } => run_wg_list_peers(&iface),
    };

    match rc {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sigil-node: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run_start() -> Result<()> {
    use anyhow::{anyhow, Context};
    use sigil_net::{
        read_transport_env, SigilNetConfig, SigilTransport, ALL_TOPICS, NETWORK_ID_STR,
        TOPIC_PEER_HEIGHTS, TOPIC_RELEASE,
    };

    let mut cfg = SigilNetConfig::default();
    // Env-driven transport override: SIGIL_TRANSPORT=direct|wireguard:<iface>|tor|wg+tor:<iface>
    cfg.transport = read_transport_env().context("parsing SIGIL_TRANSPORT")?;
    // SIGIL_P2P_PORT moves the direct-mode listen port (default 9501) so a second
    // node can run on the same host (used to verify genesis backfill locally).
    if let Ok(p) = std::env::var("SIGIL_P2P_PORT") {
        if let Ok(n) = p.trim().parse::<u16>() {
            if n != 0 { cfg.p2p_port = n; }
        }
    }
    cfg.validate()?;

    // Resolve the libp2p listen address based on the transport mode.
    let listen_addr = resolve_listen_addr(&cfg.transport, cfg.p2p_port);

    // SIGIL_NODE_ID lets an operator pin a node's identity explicitly (so two
    // nodes on different hosts get distinct, predictable PeerIds instead of
    // colliding on the default `…-node`). Falls back to HOSTNAME-derived.
    let node_id = std::env::var("SIGIL_NODE_ID").unwrap_or_else(|_| format!(
        "sigil-{}-{}",
        NETWORK_ID_STR,
        std::env::var("HOSTNAME").unwrap_or_else(|_| "node".into())
    ));
    eprintln!("⚡ sigil-node start");
    eprintln!("   node_id:         {}", node_id);
    let local_peer_id = flux_p2p::swarm::peer_id_string(&node_id);
    eprintln!("   local_peer_id:   {}", local_peer_id);
    // v0.57 (sync): publish our peer-id so a CO-LOCATED sigil-top monitor can auto-dial us over
    // loopback (tip-complete, LAN, ~0 WAN timeouts) instead of crawling the remote fleet. Same-box
    // only; the monitor confirms 127.0.0.1:9501 is live before using it. Best-effort.
    match flux_p2p::publish_sigil_peerid(&local_peer_id) {
        Ok(()) => eprintln!("   peerid published:{}", flux_p2p::sigil_peerid_path().display()),
        Err(e) => eprintln!("   (peerid publish failed: {e})"),
    }
    eprintln!("   network_id:      {}", NETWORK_ID_STR);
    eprintln!("   transport:       {}", cfg.transport.label());
    if let Some(iface) = cfg.transport.wg_interface() {
        eprintln!("   wg_interface:    {} (operator must bring up via wg-quick(8))", iface);
    }
    if cfg.transport.needs_tor() {
        #[cfg(feature = "arti")]
        eprintln!("   tor:             arti-client linked, bootstrap on first dial");
        #[cfg(not(feature = "arti"))]
        eprintln!("   tor:             ⚠ stub mode — rebuild with --features arti for real Tor");
    }
    eprintln!("   listen_addr:     {}", listen_addr);
    eprintln!("   p2p_port:        {}", cfg.p2p_port);
    eprintln!("   api_port:        {}", cfg.api_port);
    eprintln!("   db_path:         {}", cfg.db_path.display());
    eprintln!("   bootstrap_peers: {} entries", cfg.bootstrap_peers.len());
    for p in &cfg.bootstrap_peers {
        eprintln!("                    - {}", p);
    }

    // Tor-only without arti = hard error. The operator asked for Tor; they get Tor or a clear failure.
    #[cfg(not(feature = "arti"))]
    if matches!(cfg.transport, SigilTransport::Tor) {
        return Err(anyhow!(
            "SIGIL_TRANSPORT=tor selected but sigil-node was built without --features arti. \
             Rebuild with: fluxc build --package sigil-node --features sigil-net/arti"
        ));
    }

    let net_config = flux_p2p::NetworkConfig {
        node_id: node_id.clone(),
        listen_addr,
        bootstrap_peers: cfg.bootstrap_peers.clone(),
        dagknight_enabled: true,  // Track A: DAGKnight BFT consensus active
        sap_enabled: true,
        x_algo_enabled: true,
        entanglement_enabled: true, // QtFT entanglement routing active
        gossipsub_topics: ALL_TOPICS.iter().map(|s| s.to_string()).collect(),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime build")?;

    rt.block_on(async move {
        let mut mgr = flux_p2p::NetworkManager::new(net_config);
        mgr.start()
            .await
            .map_err(|e| anyhow!("flux-p2p start: {}", e))?;
        // NetworkManager is not Clone; share it across the async select loop and
        // the spawned point-to-point backfill request tasks via Arc. All of
        // publish/summary/drain_events/respond/stop/send_request take &self, so
        // the Arc is sufficient (start() above ran while still owned mutable).
        let mgr = std::sync::Arc::new(mgr);
        eprintln!("✓ flux-p2p NetworkManager started on :{}", cfg.p2p_port);
        eprintln!("  subscribed topics: {}", ALL_TOPICS.len());

        // ── Activate Tor ────────────────────────────────────────────────────
        // When the transport asks for Tor egress (`tor` / `wg+tor`), bootstrap
        // a REAL Arti client: downloads the live Tor consensus + builds an
        // entry circuit. Held in `_tor_client` for the node's lifetime so the
        // circuits stay warm; per-peer stream isolation (sigil-net-tor) is then
        // available for off-mesh dials. HONEST: this makes Tor *live in the
        // process* — routing the libp2p gossip ITSELF through Tor (a SOCKS /
        // Arti libp2p transport) is the next integration; today WG carries the
        // validator mesh and Arti stands ready for off-mesh / RPC egress.
        #[cfg(feature = "arti")]
        let _tor_client = if cfg.transport.needs_tor() {
            eprintln!("⏳ Tor: bootstrapping Arti (downloading consensus, building entry circuit)…");
            match sigil_net::TorClient::bootstrap(sigil_net::TorConfig::default()).await {
                Ok(tc) => {
                    eprintln!("✓ Tor LIVE — Arti bootstrapped, per-peer circuit isolation ready");
                    Some(tc)
                }
                Err(e) => {
                    eprintln!("🔴 Tor bootstrap failed ({e}) — continuing on the WG underlay");
                    None
                }
            }
        } else {
            None
        };

        // DEMO: prove a tiny PrivateSubmit egresses over a DEDICATED, per-layer,
        // ROTATING Tor circuit (selective-egress policy) while bulk gossip rides
        // WireGuard. Set SIGIL_TOR_DEMO_TARGET=host:port to a Tor-reachable
        // endpoint. The payload is tiny + classed PrivateSubmit, so the policy
        // routes it to Tor on circuit key `PrivateSubmit::demo-validator|e<epoch>`.
        #[cfg(feature = "arti")]
        if let (Some(tc), Ok(target)) =
            (_tor_client.as_ref(), std::env::var("SIGIL_TOR_DEMO_TARGET"))
        {
            let tc = tc.clone();
            tokio::spawn(async move {
                let mut n = 0u64;
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    tick.tick().await;
                    n += 1;
                    let payload = format!("sigil-shielded-submit#{n}").into_bytes();
                    match sigil_net::tor_policy::tor_send(
                        &tc,
                        &target,
                        "demo-validator",
                        sigil_net::EgressClass::PrivateSubmit,
                        &payload,
                    )
                    .await
                    {
                        Ok(sent) => eprintln!(
                            "🧅 PrivateSubmit #{n} → Tor: {sent}B over a dedicated rotating circuit → {target}"
                        ),
                        Err(e) => eprintln!("🧅 PrivateSubmit #{n} Tor egress failed: {e}"),
                    }
                }
            });
        }

        // Bootstrap the local chain from a deterministic genesis. Every node
        // mint-genesis call produces byte-identical block 0 (see
        // GENESIS_TIMESTAMP_MS) so block 1+ can chain across nodes.
        let mut chain = ChainTip::new();
        let snap_dir = snapshot::snapshot_dir();
        // Memory-bound persistence: an append-only on-disk block log. On boot we
        // STREAM-replay it (one block at a time → bounded RAM) to rebuild state +
        // the recent window; older blocks stay on disk. This replaced the
        // load-the-whole-chain-into-RAM aether snapshot that OOM-killed the producer.
        let mut chain_log = chain_log::ChainLog::open(&snap_dir)
            .map_err(|e| anyhow!("open chain.log: {}", e))?;
        if chain_log.height() > 0 {
            // ── v0.36.1 GENESIS GUARD L1: before touching anything, confirm the on-disk
            // chain was produced by a binary with OUR genesis. INCIDENT 2026-06-10: a
            // diverged binary read 21.9M blocks, chain.apply REJECTED every one, it resumed
            // at H=0 and APPENDED a fresh chain to the live 57GB log (data corruption).
            // Compare genesis hashes BEFORE replay; mismatch → refuse, touch nothing.
            let expected_genesis_hash = build_genesis()
                .map_err(|e| anyhow!("build_genesis (guard): {}", e))?
                .hash();
            match chain_log.get(0) {
                Some(disk_genesis) => {
                    let disk_hash = disk_genesis.hash();
                    if disk_hash != expected_genesis_hash {
                        eprintln!("\n🛑 FATAL: genesis mismatch — refusing to start. chain.log left UNTOUCHED.");
                        eprintln!("   expected (this binary): {}", hex_short_block(&expected_genesis_hash));
                        eprintln!("   on disk (chain.log):    {}", hex_short_block(&disk_hash));
                        eprintln!("   This binary is INCOMPATIBLE with the existing chain.");
                        eprintln!("   → use a matching binary, restore a backup, or archive chain.log to start fresh.\n");
                        std::process::exit(1);
                    }
                }
                None => {
                    eprintln!("\n🛑 FATAL: chain.log reports height {} but block 0 is unreadable — refusing to start.\n",
                        chain_log.height());
                    std::process::exit(1);
                }
            }
            // ── v0.36.1 SNAPSHOT BOOT: try the producer state snapshot FIRST. A valid
            // snapshot replaces the full-log replay (~35 min over 52 GB / ~21M blocks)
            // with restore + tail replay of only the blocks since the last 📸 (seconds).
            // Every failure mode (missing / corrupt / wrong chain / ahead-of-log /
            // incomplete tail) logs WHY and falls back to the full-replay path below,
            // which stays byte-for-byte unchanged.
            let t_boot = std::time::Instant::now();
            let mut booted_from_snapshot = false;
            match snapshot::load_state(&snap_dir) {
                Some(snap) if snap.snapshot_height < chain_log.height() => {
                    // Continuity gate: the snapshot's tip block must BE the chain.log
                    // block at the same height — rejects snapshots from a diverged
                    // chain or a truncated/rewritten log before any state is trusted.
                    let log_tip_hash = chain_log.get(snap.snapshot_height).map(|b| b.hash());
                    if snap.tip_block_hash() != log_tip_hash {
                        eprintln!("⚠ snapshot tip @H={} does not match chain.log at that height — falling back to full replay",
                            snap.snapshot_height);
                    } else {
                        let snap_h = snap.snapshot_height;
                        let mut restored = snap.restore();
                        let mut tail_applied: u64 = 0;
                        match chain_log::ChainLog::replay_from(&snap_dir, snap_h + 1, |b| {
                            if restored.apply(b).is_ok() { tail_applied += 1; }
                        }) {
                            Ok(tail_read) => {
                                if restored.height() == chain_log.height() {
                                    chain = restored;
                                    booted_from_snapshot = true;
                                    eprintln!("⚡ snapshot boot: H={} + tail {} blocks ({:.1}s) → resuming at H={} (window base {})",
                                        snap_h, tail_read, t_boot.elapsed().as_secs_f64(),
                                        chain.height(), chain.window_base());
                                } else {
                                    // Tail blocks rejected → restored chain is BEHIND the log.
                                    // Producing from here would append diverging heights to the
                                    // live log (the 2026-06-10 corruption mode) — full replay
                                    // instead, which hits the L2 guard if truly incompatible.
                                    eprintln!("⚠ snapshot boot incomplete: chain H={} vs log {} (tail read {}, applied {}) — falling back to full replay",
                                        restored.height(), chain_log.height(), tail_read, tail_applied);
                                }
                            }
                            Err(e) => eprintln!("⚠ snapshot tail replay failed: {} — falling back to full replay", e),
                        }
                    }
                }
                Some(snap) => {
                    eprintln!("⚠ snapshot @H={} is AHEAD of chain.log (height {}) — log truncated? ignoring snapshot, full replay",
                        snap.snapshot_height, chain_log.height());
                }
                None => {
                    eprintln!("ℹ no usable state snapshot at {} (missing or failed checksum/decode) — full replay",
                        snapshot::state_snapshot_path(&snap_dir).display());
                }
            }
            // ── GENESIS GUARD L2: count APPLIED vs READ during replay. If the log has
            // blocks but none apply, ABORT instead of silently falling through to a fresh
            // genesis that appends to the existing log (the exact incident failure mode).
            if !booted_from_snapshot {
            let mut applied: u64 = 0;
            let n = chain_log::ChainLog::replay(&snap_dir, |b| {
                if chain.apply(b).is_ok() { applied += 1; }
            }).map_err(|e| anyhow!("chain.log replay: {}", e))?;
            if n > 0 && (applied == 0 || chain.height() == 0) {
                eprintln!("\n🛑 FATAL: replay read {} blocks but applied {} (chain at H={}).", n, applied, chain.height());
                eprintln!("   Every block was REJECTED — binary incompatible with chain data.");
                eprintln!("   Refusing to re-genesis over the existing log.\n");
                std::process::exit(1);
            }
            eprintln!("♻️  RECOVERED {} blocks from chain.log (streamed, {} applied) → resuming at H={} (window base {})",
                n, applied, chain.height(), chain.window_base());
            }
        } else {
            let genesis = build_genesis().map_err(|e| anyhow!("build_genesis: {}", e))?;
            let genesis_hash = genesis.hash();
            let graw = serde_json::to_vec(&genesis).unwrap_or_default();
            chain.apply(genesis).map_err(|e| anyhow!("genesis apply: {}", e))?;
            let _ = chain_log.append_bytes(&graw);
            eprintln!("✓ chain initialised at H=0 — genesis hash {}",
                hex_short_block(&genesis_hash));
        }

        // Halt latch: once flipped, the node stops accepting blocks but
        // keeps gossipping its heartbeat so an operator can spot the halt
        // in fluxmux / log tail.
        let mut diverged = false;

        // Producer loop (opt-in via SIGIL_PRODUCER=1): mint + broadcast an
        // empty block every SIGIL_PRODUCE_MS (default 100ms) on the blocks
        // topic. Lets one node STREAM a chain so peers can measure cross-host
        // blocks/sec. Receivers count + apply (see the TOPIC_BLOCKS branch).
        let produce = std::env::var("SIGIL_PRODUCER")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let produce_ms: u64 = std::env::var("SIGIL_PRODUCE_MS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(100);
        // Sub-ms target: SIGIL_PRODUCE_US=200 → 5000 blocks/s setpoint.
        // Falls back to produce_ms×1000; floor 50µs so we don't spin the core.
        let produce_us: u64 = std::env::var("SIGIL_PRODUCE_US")
            .ok().and_then(|v| v.parse().ok())
            .unwrap_or_else(|| produce_ms.saturating_mul(1000).max(200));
        let feed_every: u64 = std::env::var("SIGIL_FEED_EVERY")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
        // Grace after first peer before minting block 1 — lets the gossipsub
        // mesh GRAFT so the receiver actually gets H=1 onward (otherwise it
        // joins mid-stream and gaps forever, Phase 0 has no backfill).
        let grace_ms: u64 = std::env::var("SIGIL_PRODUCE_GRACE_MS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(4000);
        // 📸 v0.36.1: periodic producer state snapshot every SIGIL_SNAPSHOT_EVERY
        // blocks (default 100_000 ≈ 10 min at ~156 blk/s; 0 disables). The capture
        // (clone of state + 8192-block window) happens on the producer tick; the
        // serialize + atomic write run on a detached OS thread so the few-MB disk
        // write never stalls block production (tick uses MissedTickBehavior::Skip
        // anyway). `snap_inflight` prevents overlapping writers on the shared
        // state-snapshot.tmp path.
        let snapshot_every: u64 = std::env::var("SIGIL_SNAPSHOT_EVERY")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);
        let snap_inflight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if produce && snapshot_every > 0 {
            eprintln!("📸 state snapshot every {} blocks → {}",
                snapshot_every, snapshot::state_snapshot_path(&snap_dir).display());
        }
        let mut first_peer_at: Option<std::time::Instant> = None;
        let mut producing = false;
        // Short producer tag for the per-block dag.html feed line.
        let prod_tag = if node_id.contains("eps") { "E" }
            else if node_id.contains("delta") { "D" }
            else if node_id.contains("gamma") { "G" }
            else if node_id.contains("beta") { "B" } else { "?" };

        // ── verify-once mempool + tx load-gen (Stargate #3 → real TPS) ───────
        // SIGIL_TXGEN=N packs up to N verify-once ed25519 txs per block. The
        // gen task SIGNS + the mempool VERIFIES each tx exactly once on ingest;
        // the producer PULLS verified txs (no re-verify) and commits their
        // count+root into the header. N=0 (default) keeps empty-block behaviour.
        let txgen: usize = std::env::var("SIGIL_TXGEN")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let mempool: Arc<Mutex<Mempool>> = Arc::new(Mutex::new(Mempool::new()));
        if txgen > 0 {
            eprintln!("💳 TXGEN — packing up to {txgen} verify-once ed25519 txs/block");
            let mp = Arc::clone(&mempool);
            // a fixed pool of hot-path keypairs; vary the amount so each tx hash
            // is distinct (no dedup collisions) while signing stays cheap.
            let keys: Vec<([u8; 32], [u8; 32], [u8; 32])> =
                (0..256).map(|_| ed25519_keygen()).collect();
            // Dedicated OS thread (not tokio) so the parallel signing burst never
            // blocks the async runtime / producer loop. SIGIL_TXGEN_THREADS caps
            // the sign fan-out (leave headroom for Quillon on shared boxes).
            let sign_threads: usize = std::env::var("SIGIL_TXGEN_THREADS")
                .ok().and_then(|v| v.parse().ok())
                .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
            eprintln!("💳 TXGEN signer threads: {sign_threads}");
            std::thread::spawn(move || {
                let mut amount: u128 = 1;
                let target = txgen * 3; // keep ~3 blocks buffered, bound memory
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let need = {
                        let g = mp.lock().unwrap();
                        if g.len() >= target { 0 } else { target - g.len() }
                    };
                    if need == 0 { continue; }
                    // SIGN in parallel across cores — disjoint amount ranges keep
                    // every tx hash unique (no dedup collisions). This was the
                    // load-gen bottleneck; verify-once has far more headroom.
                    let base = amount;
                    let chunk = need.div_ceil(sign_threads);
                    let batch: Vec<SignedTx> = std::thread::scope(|s| {
                        let mut hs = Vec::new();
                        for c in 0..sign_threads {
                            let lo = c * chunk; let hi = (lo + chunk).min(need);
                            if lo >= hi { continue; }
                            let keys = &keys;
                            hs.push(s.spawn(move || {
                                let mut out = Vec::with_capacity(hi - lo);
                                for i in lo..hi {
                                    let amt = base + i as u128;
                                    let (sk, pk, from) = &keys[(amt as usize) % keys.len()];
                                    let tx = SigilTx::Send { from: *from, to: [0u8; 32], amount: amt, token: [0u8; 32], fee: 1 };
                                    out.push(ed25519_sign_tx(tx, sk, pk));
                                }
                                out
                            }));
                        }
                        let mut all = Vec::with_capacity(need);
                        for h in hs { all.extend(h.join().unwrap()); }
                        all
                    });
                    amount += need as u128;
                    // ingest VERIFIES once, batch×parallel (the wall we are measuring)
                    mp.lock().unwrap().ingest(batch);
                }
            });
        }

        // DagKnight v0: in DAG mode, both nodes produce, and each block
        // references the peer's latest tips as MERGE PARENTS (parallel, not
        // linear). The receiver records peer block hashes as tips instead of
        // strict-linear-applying them (two producers would otherwise fork).
        let dag_mode = std::env::var("SIGIL_DAG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
        let mut peer_tips: std::collections::VecDeque<BlockHash> = std::collections::VecDeque::new();
        if dag_mode {
            eprintln!("🕸 DAG mode — blocks merge peer tips as parents (DagKnight v0)");
        }
        let mut produce_tick =
            tokio::time::interval(std::time::Duration::from_micros(produce_us.max(50)));
        produce_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut produced: u64 = 0;
        let mut received: u64 = 0;
        let mut applied: u64 = 0;
        let mut tx_total: u64 = 0;   // verify-once txs seen in received blocks (TPS meter)
        let mut produced_tx: u64 = 0; // verify-once txs this node packed into its own blocks
        let t_start = std::time::Instant::now();
        // ── genesis backfill (closes the Phase-0 "joins mid-stream, gaps forever" gap) ──
        // Out-of-order / future blocks are buffered by height and applied contiguously
        // as the tip advances; on a gap we ask ONE connected peer over the flux-p2p
        // request-response channel (point-to-point, no flood) for the missing range.
        // The peer answers from its chain with a BackfillResp; we feed the blocks
        // back into `pending` via `bf_rx` (the request is awaited off the select loop
        // in a spawned task so it never blocks production/drain).
        let mut pending: std::collections::BTreeMap<u64, crate::block::Block> = std::collections::BTreeMap::new();
        let (bf_tx, mut bf_rx) = tokio::sync::mpsc::channel::<Vec<serde_json::Value>>(64);
        // Throttle gap requests so a sustained gap doesn't spawn a request task on
        // every received future block (fire at most every ~300ms).
        let mut last_req = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(10)).unwrap_or_else(std::time::Instant::now);
        let mut backfilled: u64 = 0;
        if produce {
            eprintln!("🏭 PRODUCER mode — target {:.0} blocks/s ({}µs tick, feed every {}) on {}",
                1_000_000.0 / produce_us as f64, produce_us, feed_every, sigil_net::TOPIC_BLOCKS);
        }

        // Drain every 250ms (chronos-tuned) so request-response serving + apply stay
        // responsive and spread out. Safe now that backfill is point-to-point (no
        // gossip re-broadcast flood); heartbeat is gated to 5s below.
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        let mut last_heartbeat = std::time::Instant::now();
        // Rate-limit EXPENSIVE full-block serves (≤1 per 120ms) so catch-up backfill
        // to behind followers can't saturate the single-threaded loop and starve block
        // production. Headers-only serves (cheap, for the monitor) are NOT throttled.
        let mut last_full_serve = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1)).unwrap_or_else(std::time::Instant::now);
        // v0.33.6 AETHER CACHE-SERVE: epsilon is the sole deep-history server and is overloaded
        // (produce ~151 blk/s + serve 3 catching-up followers + light clients), re-reading disk +
        // re-serializing + re-zstd'ing the SAME finalized ranges over and over (the journal shows
        // identical [from..to] served to multiple peers seconds apart). Cache the exact response
        // bytes for IMMUTABLE ranges (hi < window_base, finalized → never change) keyed by the
        // request shape; a hit is a pure memcpy that bypasses the 120ms throttle. FIFO-capped so
        // memory stays bounded (~CAP chunks of recent finalized history).
        let mut serve_cache: std::collections::HashMap<(u64, u64, bool, u32), std::sync::Arc<Vec<u8>>> =
            std::collections::HashMap::new();
        let mut serve_cache_order: std::collections::VecDeque<(u64, u64, bool, u32)> =
            std::collections::VecDeque::new();
        const SERVE_CACHE_CAP: usize = 2048; // ~2048 finalized chunks hot in RAM
        let (mut cache_hits, mut cache_miss): (u64, u64) = (0, 0);
        let mut last_cache_log = std::time::Instant::now();
        loop {
            tokio::select! {
                _ = produce_tick.tick(), if produce => {
                    // Gate production on (a) having a peer and (b) a grace
                    // period after the first peer so the gossipsub mesh grafts
                    // — otherwise the receiver joins mid-stream and gaps
                    // forever (Phase 0 has no backfill). Once grace elapses,
                    // both advance from H=1 in lockstep.
                    let peers = mgr.summary().peer_count;
                    if peers == 0 {
                        first_peer_at = None;
                        producing = false;
                    } else if first_peer_at.is_none() {
                        first_peer_at = Some(std::time::Instant::now());
                        eprintln!("🤝 peer connected — minting block 1 in {}ms (mesh-graft grace)", grace_ms);
                    } else if !producing
                        && first_peer_at.map(|t| t.elapsed().as_millis() as u64 >= grace_ms).unwrap_or(false)
                    {
                        producing = true;
                        eprintln!("🏭 grace elapsed — streaming blocks now");
                    }
                    if producing {
                    let mp: Vec<BlockHash> =
                        if dag_mode { peer_tips.iter().cloned().collect() } else { Vec::new() };
                    // pull verify-once txs (already verified at mempool ingest)
                    let block_txs: Vec<SignedTx> =
                        if txgen > 0 { mempool.lock().unwrap().pull(txgen) } else { Vec::new() };
                    match mint_next_block(&chain, mp, &block_txs) {
                        Ok(block) => {
                            let h = block.header.height;
                            let bhash = block.hash();
                            let parent = block.header.parent_hash;
                            let mps = block.header.merge_parents.clone();
                            // Real tip-proof material: the block's HEADER roots (what the
                            // producer attests) + full hash, captured BEFORE apply moves the
                            // block. Using header roots — not chain.roots() post-apply, which
                            // zeroes event_log_root. Lets the light client verify the REAL tip.
                            let header_roots = sigil_state::StateRoots {
                                wallet_state_root:   block.header.wallet_state_root,
                                dex_state_root:      block.header.dex_state_root,
                                event_log_root:      block.header.event_log_root,
                                contract_state_root: block.header.contract_state_root,
                            };
                            let roots_json = serde_json::to_string(&header_roots).unwrap_or_else(|_| "null".into());
                            let tiphash = hex_full(&bhash);
                            let bytes = serde_json::to_vec(&block).unwrap_or_default();
                            // advance our own tip first, then broadcast it
                            match chain.apply(block) {
                                Ok(_) => {
                                    let _ = chain_log.append_bytes(&bytes); // durable, O(1)
                                    produced += 1;
                                    produced_tx += block_txs.len() as u64;
                                    // per-block feed line for dag.html (stdout; the
                                    // dag-feed sidecar tails these into dag-blocks.json).
                                    // parents[0] = selected parent, rest = DAG merge parents.
                                    let mut ps = vec![format!("\"{}\"", hex_short_block(&parent))];
                                    for m in &mps { ps.push(format!("\"{}\"", hex_short_block(m))); }
                                    if produced == 1 || produced % feed_every == 0 {

                                        println!("📦{{\"h\":{},\"hash\":\"{}\",\"parents\":[{}],\"prod\":\"{}\",\"tiphash\":\"{}\",\"roots\":{}}}",

                                            h, hex_short_block(&bhash), ps.join(","), prod_tag, tiphash, roots_json);

                                    }
                                    // Selective-egress policy governs the real
                                    // send path: block gossip is HotMesh → stays
                                    // on the fast WireGuard mesh, never Tor.
                                    if produced == 1 {
                                        eprintln!("📋 egress policy: block gossip = HotMesh → {:?} (bulk never rides Tor)",
                                            sigil_net::route_egress(sigil_net::EgressClass::HotMesh, bytes.len()));
                                    }
                                    if let Err(e) = mgr.publish(sigil_net::TOPIC_BLOCKS, bytes) {
                                        eprintln!("⚠ publish block H={} failed: {}", h, e);
                                    }
                                    if produced % 100 == 0 {
                                        let secs = t_start.elapsed().as_secs_f64().max(1e-6);
                                        eprintln!("🏭 produced {} blocks ({:.1}/s) · {} txs ({:.0} TPS verify-once) — tip H={}",
                                            produced, produced as f64 / secs,
                                            produced_tx, produced_tx as f64 / secs, chain.height());
                                    }
                                    // Persistence is now per-block via chain_log.append_bytes
                                    // above (O(1), bounded RAM) — no periodic full-chain
                                    // snapshot (that was O(N²) + the OOM source).
                                    // 📸 v0.36.1: periodic STATE snapshot (bounded — window +
                                    // accumulated state, NOT the chain) so the next boot is
                                    // restore + tail replay instead of a 35-min full replay.
                                    if snapshot_every > 0 && chain.height() % snapshot_every == 0
                                        && !snap_inflight.swap(true, std::sync::atomic::Ordering::SeqCst)
                                    {
                                        match snapshot::StateSnapshot::capture(&chain) {
                                            Some(snap) => {
                                                let dir = snap_dir.clone();
                                                let flag = std::sync::Arc::clone(&snap_inflight);
                                                std::thread::spawn(move || {
                                                    let t0 = std::time::Instant::now();
                                                    match snapshot::save_state(&snap, &dir) {
                                                        Ok(bytes) => eprintln!(
                                                            "📸 state snapshot @ H={} ({} B, {:.2}s write, off hot path)",
                                                            snap.snapshot_height, bytes, t0.elapsed().as_secs_f64()),
                                                        Err(e) => eprintln!(
                                                            "⚠ state snapshot @ H={} failed: {} (boot will full-replay)",
                                                            snap.snapshot_height, e),
                                                    }
                                                    flag.store(false, std::sync::atomic::Ordering::SeqCst);
                                                });
                                            }
                                            None => { snap_inflight.store(false, std::sync::atomic::Ordering::SeqCst); }
                                        }
                                    }
                                }
                                Err(e) => eprintln!("⚠ producer self-apply H={} failed: {}", h, e),
                            }
                        }
                        Err(e) => eprintln!("⚠ mint_next_block failed: {}", e),
                    }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("⏹ sigil-node — SIGINT received, shutting down");
                    let _ = mgr.stop().await;
                    return Ok::<(), anyhow::Error>(());
                }
                _ = tick.tick() => {
                    // Heartbeat + peer-height publish stays on a 5s cadence.
                    if last_heartbeat.elapsed() >= std::time::Duration::from_secs(5) {
                        last_heartbeat = std::time::Instant::now();
                        let sum = mgr.summary();
                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0);
                        let hb = serde_json::json!({
                            "node":     node_id,
                            "network":  NETWORK_ID_STR,
                            "ts":       ts,
                            "peers":    sum.peer_count,
                            "started":  sum.started,
                            "height":   chain.height(),
                        });
                        let bytes = serde_json::to_vec(&hb).unwrap_or_default();
                        if let Err(e) = mgr.publish(TOPIC_PEER_HEIGHTS, bytes) {
                            eprintln!("⚠ publish peer-heights failed: {}", e);
                        }
                        eprintln!("⚡ heartbeat — peers={} started={}", sum.peer_count, sum.started);
                    }

                    // Drain incoming events from peers (every 250ms): live block gossip on
                    // TOPIC_BLOCKS + point-to-point backfill requests (InboundRequest).
                    for ev in mgr.drain_events() {
                        match ev {
                        flux_p2p::SwarmAppEvent::InboundRequest { peer, request_id, payload } => {
                            // Point-to-point backfill serve: answer ONE requester with
                            // the requested block range straight from our chain. No
                            // gossipsub re-broadcast — the response goes only to `peer`.
                            let req: BackfillReq = match serde_json::from_slice(&payload) {
                                Ok(r) => r,
                                Err(_) => continue,
                            };
                            let top = chain.height().saturating_sub(1);
                            let lo = req.from;
                            // point-to-point ⇒ a bigger chunk is fine.
                            // v0.56 (sync throughput): headers are ~70 B, so a headers_only chunk
                            // can be FAR larger than the full-block cap (32768 headers ≈ 2.3 MB raw,
                            // ~160 KB zstd) and cut round-trips on a thin/lossy mesh — the
                            // ~200 headers/s bottleneck was the round-trip-bound 8192 cap. Full-block
                            // serves stay at 8192 (≈8 MB — the WAN-burst-stall limit). Env-tunable.
                            let serve_cap: u64 = if req.headers_only {
                                std::env::var("SIGIL_SERVE_HEADERS_CAP").ok()
                                    .and_then(|v| v.parse::<u64>().ok())
                                    .map(|n| n.clamp(8192, 262_144)).unwrap_or(32_768)
                            } else { 8192 };
                            let hi = req.to.min(top).min(lo.saturating_add(serve_cap));
                            let wbase = chain.window_base();
                            // v0.33.6: a range entirely BELOW the live window is FINALIZED — its
                            // bytes never change, so cache + replay them. The tip/window chunk is
                            // excluded (it still mutates).
                            let immutable = hi >= lo && hi < wbase;
                            let ckey = (lo, hi, req.headers_only, req.codec as u32);

                            // ── CACHE HIT: pure memcpy, bypasses the throttle entirely ──
                            if immutable {
                                if let Some(blob) = serve_cache.get(&ckey) {
                                    cache_hits += 1;
                                    mgr.respond(request_id, blob.as_ref().clone());
                                    if last_cache_log.elapsed() >= std::time::Duration::from_secs(5) {
                                        let tot = (cache_hits + cache_miss).max(1);
                                        eprintln!("⚡ serve-cache: {} hits / {} miss ({:.0}% hit) · {} chunks RAM",
                                            cache_hits, cache_miss, cache_hits as f64 * 100.0 / tot as f64, serve_cache.len());
                                        last_cache_log = std::time::Instant::now();
                                    }
                                    continue;
                                }
                            }

                            // Throttle costly full-block MISSES so catch-up backfill can't starve
                            // production; the requester re-asks on its own cadence. (Hits already
                            // returned above — they're free.)
                            if !req.headers_only {
                                if last_full_serve.elapsed() < std::time::Duration::from_millis(120) {
                                    let empty = BackfillResp { blocks: Vec::new() };
                                    mgr.respond(request_id, serde_json::to_vec(&empty).unwrap_or_default());
                                    continue;
                                }
                                last_full_serve = std::time::Instant::now();
                            }
                            if immutable { cache_miss += 1; }

                            // Gather the range: disk portion via ONE sequential read
                            // (chain_log.get_range), recent portion from the in-RAM window.
                            let mut blocks: Vec<crate::block::Block> = Vec::new();
                            if lo < wbase {
                                let disk_hi = hi.min(wbase.saturating_sub(1));
                                blocks.extend(chain_log.get_range(lo, disk_hi));
                            }
                            for h in lo.max(wbase)..=hi {
                                if let Some(b) = chain.get(h) { blocks.push(b.clone()); }
                            }
                            let out: Vec<u8> = if req.headers_only {
                                // monitor path: bincode Vec<header>, ~20× smaller, no JSON.
                                let headers: Vec<_> = blocks.iter().map(|b| b.header.clone()).collect();
                                let body = bincode::serialize(&headers).unwrap_or_default();
                                // v0.33 zstd wire: codec=1 → 'Z' + zstd-1(body). Measured 14.0×
                                // on a real chunk (~20 ms/4 MB — far cheaper than the wire time
                                // it saves). Any compress error falls back to plain 'H'.
                                let out = if req.codec == 1 {
                                    match zstd::encode_all(&body[..], 1) {
                                        Ok(z) => { let mut o = vec![b'Z']; o.extend(z); o }
                                        Err(_) => { let mut o = vec![b'H']; o.extend(&body); o }
                                    }
                                } else {
                                    let mut o = vec![b'H']; o.extend(&body); o
                                };
                                eprintln!("↩ rr-backfill: served {} HEADERS [{}..={}] to {} ({} B, codec {})",
                                    headers.len(), lo, hi, peer, out.len(),
                                    if out.first() == Some(&b'Z') { "zstd" } else { "raw" });
                                out
                            } else {
                                let resp = BackfillResp {
                                    blocks: blocks.iter().map(|b| serde_json::to_value(b).unwrap()).collect(),
                                };
                                eprintln!("↩ rr-backfill: served {} blocks [{}..={}] to {}",
                                    resp.blocks.len(), lo, hi, peer);
                                serde_json::to_vec(&resp).unwrap_or_default()
                            };
                            // ── CACHE FILL (finalized ranges only), FIFO-capped ──
                            if immutable && !out.is_empty() {
                                serve_cache.insert(ckey, std::sync::Arc::new(out.clone()));
                                serve_cache_order.push_back(ckey);
                                while serve_cache_order.len() > SERVE_CACHE_CAP {
                                    if let Some(k) = serve_cache_order.pop_front() { serve_cache.remove(&k); }
                                }
                            }
                            mgr.respond(request_id, out);
                        }
                        flux_p2p::SwarmAppEvent::GossipsubMessage {
                            topic, from, data, ..
                        } => {
                            if topic == sigil_net::TOPIC_BLOCKS {
                                // Receiver: COUNT every block that arrives (the
                                // cross-host throughput number), then apply in
                                // order. Gossipsub is unordered + at-most-once
                                // in P0, so a height GAP under load is expected
                                // — count + skip those (don't apply, don't
                                // halt). We ONLY halt on a TRUE root-divergence
                                // at the CORRECT height.
                                if diverged { continue; }
                                // TOPIC_BLOCKS now carries ONLY live blocks; backfill
                                // moved to the point-to-point request-response channel
                                // (see the InboundRequest arm + the gap-request task).
                                // Anything that isn't a block is ignored here.
                                let block: crate::block::Block = match serde_json::from_slice(&data) {
                                    Ok(b) => b,
                                    Err(_) => continue,
                                };
                                received += 1;
                                if dag_mode {
                                    peer_tips.push_back(block.hash());
                                    while peer_tips.len() > 4 { peer_tips.pop_front(); }
                                    tx_total += block.header.tx_count as u64;
                                    if received % 200 == 0 {
                                        let secs = t_start.elapsed().as_secs_f64().max(1e-6);
                                        eprintln!("🕸 merged {} peer blocks ({:.1}/s) · {} verify-once txs ({:.0} TPS) as DAG tips",
                                            received, received as f64 / secs, tx_total, tx_total as f64 / secs);
                                    }
                                    continue;
                                }
                                let h = block.header.height;
                                let expected = chain.height();
                                if h < expected {
                                    continue; // already applied
                                }
                                if h > expected {
                                    // Future block: buffer it, then ask ONE connected
                                    // peer point-to-point (request-response) for the
                                    // missing range. send_request is async + awaits, so
                                    // we spawn it off the select loop and feed the answer
                                    // back through bf_rx — never blocking production/drain.
                                    if pending.len() < 200_000 {
                                        pending.entry(h).or_insert(block);
                                    }
                                    if last_req.elapsed() >= std::time::Duration::from_millis(300) {
                                        last_req = std::time::Instant::now();
                                        if let Some(peer) = mgr.connected_peers().into_iter().next() {
                                            let req = BackfillReq { from: expected, to: expected.saturating_add(8192), headers_only: false, codec: 0 };
                                            eprintln!("⇪ rr-backfill: gap (have {}, saw {}) — requesting [{}..={}] from {}",
                                                expected, h, req.from, req.to, peer);
                                            let mgr2 = std::sync::Arc::clone(&mgr);
                                            let bf_tx2 = bf_tx.clone();
                                            tokio::spawn(async move {
                                                let payload = match serde_json::to_vec(&req) {
                                                    Ok(p) => p,
                                                    Err(_) => return,
                                                };
                                                if let Ok(bytes) = mgr2.send_request(peer, payload).await {
                                                    if let Ok(resp) = serde_json::from_slice::<BackfillResp>(&bytes) {
                                                        let _ = bf_tx2.send(resp.blocks).await;
                                                    }
                                                }
                                            });
                                        }
                                    }
                                    continue;
                                }
                                // h == expected: apply, then drain contiguous buffered blocks.
                                let mut next = Some(block);
                                while let Some(b) = next.take() {
                                    let bh = b.header.height;
                                    let braw = serde_json::to_vec(&b).unwrap_or_default();
                                    match chain.apply(b) {
                                        Ok(_) => {
                                            let _ = chain_log.append_bytes(&braw);
                                            applied += 1;
                                            if bh != h { backfilled += 1; }
                                            if applied % 100 == 0 {
                                                let secs = t_start.elapsed().as_secs_f64().max(1e-6);
                                                eprintln!("✓ applied {} blocks ({:.1}/s) — recv {} — backfilled {} — tip H={} — buffered {}",
                                                    applied, applied as f64 / secs, received, backfilled, chain.height(), pending.len());
                                            }
                                            next = pending.remove(&chain.height());
                                        }
                                        Err(e) => {
                                            eprintln!("🔴 STATE DIVERGENCE at H={} from {} — {}", bh, from, e);
                                            diverged = true;
                                            fire_chain_event(
                                                "divergence",
                                                &serde_json::json!({
                                                    "node": node_id,
                                                    "height": bh,
                                                    "from": from.to_string(),
                                                    "error": format!("{e}"),
                                                    "exit_code": 78,
                                                }),
                                            );
                                            break;
                                        }
                                    }
                                }
                            } else if topic == TOPIC_RELEASE {
                                // Hand bytes to sigil-updater. transport::handle_release_message
                                // parses + verifies + fetches + applies in one call. We pass
                                // env!("CARGO_PKG_VERSION") as current_version so the version
                                // gate only swaps strictly-newer binaries, and current_exe()
                                // as target so the swap lands on this binary's path.
                                //
                                // Phase 0 limitation: this fires the swap immediately on
                                // verify; activation_height gating against the chain's tip
                                // lands once sigil-node has a real chain-height source. For
                                // now the producer's activation_height is checked for
                                // freshness by verify_announcement (>= current_height-style
                                // checks belong to P1).
                                use sigil_updater::{handle_release_message, CurlFetcher, HandledRelease};
                                let from_str = format!("{}", from);
                                let target = match std::env::current_exe() {
                                    Ok(p) => p,
                                    Err(e) => {
                                        eprintln!("⚠ RELEASE from {}: cannot resolve current_exe — {}", from, e);
                                        continue;
                                    }
                                };
                                let fetcher = CurlFetcher::default();
                                let trusted = trusted_release_keys();
                                if trusted.is_empty() {
                                    eprintln!(
                                        "🔒 RELEASE from {}: auto-update is OFF (no pinned release keys); set TRUSTED_RELEASE_KEYS_HEX or SIGIL_TRUSTED_RELEASE_KEYS to enable. Ignoring.",
                                        from
                                    );
                                    continue;
                                }
                                let outcome = handle_release_message(
                                    &data,
                                    Some(&from_str),
                                    &trusted,
                                    env!("CARGO_PKG_VERSION"),
                                    &target,
                                    &fetcher,
                                );
                                match outcome {
                                    Ok(HandledRelease::Applied { verify, outcome }) => {
                                        eprintln!(
                                            "🚀 RELEASE from {} APPLIED — {} v{} bytes={} → {} (previous backed up: {})",
                                            from, verify.product, verify.version,
                                            verify.binary_size_bytes,
                                            outcome.target.display(),
                                            outcome.previous_existed,
                                        );
                                        eprintln!(
                                            "   activation_height={}, min_consensus_version={}",
                                            verify.activation_height, verify.min_consensus_version,
                                        );
                                        eprintln!(
                                            "   ⚠ binary swapped on disk; respawn deferred to P1 (activation_height enforcement)"
                                        );
                                    }
                                    Ok(HandledRelease::NotAnAnnouncement { reason }) => {
                                        eprintln!("⚠ RELEASE from {}: not a parseable announcement — {}", from, reason);
                                    }
                                    Ok(HandledRelease::VerifyFailed { error, .. }) => {
                                        eprintln!("🔴 RELEASE from {} FAILED VERIFY: {}", from, error);
                                    }
                                    Ok(HandledRelease::NotNewer { announcement_version, current_version }) => {
                                        eprintln!(
                                            "✓ RELEASE from {}: v{} not newer than v{} (skipped)",
                                            from, announcement_version, current_version,
                                        );
                                    }
                                    Ok(HandledRelease::FetchFailed { url, error }) => {
                                        eprintln!("🔴 RELEASE from {} fetch failed ({}): {}", from, url, error);
                                    }
                                    Ok(HandledRelease::BinaryHashMismatch { url, error }) => {
                                        eprintln!("🔴 RELEASE from {} hash mismatch ({}): {}", from, url, error);
                                    }
                                    Err(e) => {
                                        eprintln!("🔴 RELEASE from {} apply error: {}", from, e);
                                    }
                                }
                            } else if topic == TOPIC_PEER_HEIGHTS {
                                // Self-healing backfill trigger: a peer announces its chain
                                // height here. If it's ahead of us we proactively pull the
                                // gap — even if we receive NO live blocks (e.g. not grafted
                                // into the block-gossip mesh after a restart/rejoin). This is
                                // what makes a connected-but-idle node recover on its own.
                                if diverged { continue; }
                                let peer_h = serde_json::from_slice::<serde_json::Value>(&data)
                                    .ok()
                                    .and_then(|v| v.get("height").and_then(|x| x.as_u64()))
                                    .unwrap_or(0);
                                let expected = chain.height();
                                if peer_h > expected
                                    && last_req.elapsed() >= std::time::Duration::from_millis(300)
                                {
                                    last_req = std::time::Instant::now();
                                    if let Some(peer) = mgr.connected_peers().into_iter().next() {
                                        let req = BackfillReq {
                                            from: expected,
                                            to: expected.saturating_add(8192),
                                            headers_only: false,
                                            codec: 0, // node-to-node needs full blocks; raw JSON path
                                        };
                                        eprintln!("⇪ rr-backfill: behind via peer-heights (have {}, net {}) — requesting [{}..={}]",
                                            expected, peer_h, req.from, req.to);
                                        let mgr2 = std::sync::Arc::clone(&mgr);
                                        let bf_tx2 = bf_tx.clone();
                                        tokio::spawn(async move {
                                            if let Ok(payload) = serde_json::to_vec(&req) {
                                                if let Ok(bytes) = mgr2.send_request(peer, payload).await {
                                                    if let Ok(resp) = serde_json::from_slice::<BackfillResp>(&bytes) {
                                                        let _ = bf_tx2.send(resp.blocks).await;
                                                    }
                                                }
                                            }
                                        });
                                    }
                                }
                            } else {
                                let preview = std::str::from_utf8(&data)
                                    .map(|s| s.chars().take(120).collect::<String>())
                                    .unwrap_or_else(|_| format!("<{} bytes>", data.len()));
                                eprintln!("📨 {} from {} — {}", topic, from, preview);
                            }
                        }
                        _ => {}
                        }
                    }
                }
                Some(vals) = bf_rx.recv() => {
                    // Point-to-point backfill response arrived: buffer each block by
                    // height, then drain `pending` contiguously into the chain via the
                    // same apply path the live-block branch uses.
                    if !diverged {
                        for v in vals {
                            if let Ok(block) = serde_json::from_value::<crate::block::Block>(v) {
                                let h = block.header.height;
                                if h >= chain.height() {
                                    pending.entry(h).or_insert(block);
                                }
                            }
                        }
                        // Apply every contiguous block we now have, starting at the tip.
                        while let Some(b) = pending.remove(&chain.height()) {
                            let bh = b.header.height;
                            let braw = serde_json::to_vec(&b).unwrap_or_default();
                            match chain.apply(b) {
                                Ok(_) => {
                                    let _ = chain_log.append_bytes(&braw);
                                    applied += 1;
                                    backfilled += 1;
                                    if applied % 100 == 0 {
                                        let secs = t_start.elapsed().as_secs_f64().max(1e-6);
                                        eprintln!("✓ applied {} blocks ({:.1}/s) — recv {} — backfilled {} — tip H={} — buffered {}",
                                            applied, applied as f64 / secs, received, backfilled, chain.height(), pending.len());
                                    }
                                }
                                Err(e) => {
                                    eprintln!("🔴 STATE DIVERGENCE at H={} (rr-backfill) — {}", bh, e);
                                    diverged = true;
                                    fire_chain_event(
                                        "divergence",
                                        &serde_json::json!({
                                            "node": node_id,
                                            "height": bh,
                                            "from": "rr-backfill",
                                            "error": format!("{e}"),
                                            "exit_code": 78,
                                        }),
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    })?;

    Ok(())
}

/// `sigil-node snapshot-create` — rebuild state by FULL chain.log replay (the
/// state only exists by applying blocks, so creating a snapshot out-of-band
/// costs one full replay — same as the boot this snapshot will save), then
/// write `<dir>/state-snapshot.bin` atomically. Run it once on a stopped
/// producer to convert the NEXT boot from ~35 min to seconds; after that the
/// running producer keeps the snapshot fresh every SIGIL_SNAPSHOT_EVERY blocks.
fn run_snapshot_create() -> Result<()> {
    let snap_dir = snapshot::snapshot_dir();
    eprintln!("⏳ snapshot-create: full replay of {} (state must be rebuilt by applying blocks — this takes as long as a normal boot)",
        snap_dir.join("chain.log").display());
    let t0 = std::time::Instant::now();
    let mut chain = ChainTip::new();
    let mut applied: u64 = 0;
    let n = chain_log::ChainLog::replay(&snap_dir, |b| {
        if chain.apply(b).is_ok() { applied += 1; }
    })
    .map_err(|e| anyhow!("chain.log replay: {}", e))?;
    if n == 0 {
        return Err(anyhow!("no blocks in {} — nothing to snapshot", snap_dir.join("chain.log").display()));
    }
    if applied != n {
        eprintln!("⚠ replay read {} blocks but applied only {} — snapshot captures state at H={}",
            n, applied, chain.height().saturating_sub(1));
    }
    let replay_secs = t0.elapsed().as_secs_f64();
    let snap = snapshot::StateSnapshot::capture(&chain)
        .ok_or_else(|| anyhow!("chain is empty after replay — nothing to snapshot"))?;
    let t1 = std::time::Instant::now();
    let bytes = snapshot::save_state(&snap, &snap_dir).context("writing state snapshot")?;
    println!("📸 state snapshot written: {}", snapshot::state_snapshot_path(&snap_dir).display());
    println!("   snapshot_height: {}", snap.snapshot_height);
    println!("   window:          [{}..={}] ({} blocks)", snap.base_height, snap.snapshot_height, snap.blocks.len());
    println!("   size:            {} bytes", bytes);
    println!("   replay:          {:.1}s ({} blocks) · write: {:.2}s", replay_secs, n, t1.elapsed().as_secs_f64());
    Ok(())
}

/// `sigil-node snapshot-info` — print height / size / checksum status of the
/// current state-snapshot file. Read-only, instant.
fn run_snapshot_info() -> Result<()> {
    let snap_dir = snapshot::snapshot_dir();
    let path = snapshot::state_snapshot_path(&snap_dir);
    println!("snapshot file: {}", path.display());
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => {
            println!("status:        MISSING (boot will full-replay; create via `sigil-node snapshot-create`)");
            return Ok(());
        }
    };
    println!("size:          {} bytes", meta.len());
    match snapshot::load_state(&snap_dir) {
        Some(s) => {
            println!("checksum:      OK (BLAKE3 verified)");
            println!("version:       {}", s.version);
            println!("height:        {}", s.snapshot_height);
            println!("window:        [{}..={}] ({} blocks)", s.base_height, s.snapshot_height, s.blocks.len());
            if let Some(h) = s.tip_block_hash() {
                println!("tip hash:      {}", hex_full(&h));
            }
        }
        None => {
            println!("checksum:      FAILED (corrupt, torn, or version-mismatched — boot will full-replay)");
        }
    }
    Ok(())
}

fn run_show_tip() -> Result<()> {
    // Phase 0: in-memory chain is empty unless this process produced it. Real
    // tip lookup goes through flux-db when storage lands.
    let chain = ChainTip::new();
    println!("height:      {}", chain.height());
    println!("parent_hash: {}", hex_full(&chain.parent_hash()));
    let r = chain.roots();
    println!("wallet_root: {}", hex_full(&r.wallet_state_root));
    println!("dex_root:    {}", hex_full(&r.dex_state_root));
    println!("event_root:  {}", hex_full(&r.event_log_root));
    println!("contract_root: {}", hex_full(&r.contract_state_root));
    Ok(())
}

fn run_mint_genesis() -> Result<()> {
    let mut chain = ChainTip::new();
    let block = build_genesis()?;
    let hash = block.hash();
    chain.apply(block)?;
    println!("✓ minted + applied genesis block");
    println!("  height: 0");
    println!("  hash:   {}", hex_full(&hash));
    println!("  tip:    height={}, parent={}", chain.height(), hex_full(&chain.parent_hash()));
    Ok(())
}

/// Demo pipeline: genesis → apply N signed txs → block 1 → apply → print tip.
/// With `broadcast=true` the block is also published on the /sigil/g0/blocks
/// gossipsub topic before exit. Phase 0 only; real producer loop comes with
/// mempool + consensus crates.
fn run_produce_block(tx_file: &str, broadcast: bool, dry_run: bool) -> Result<()> {
    // 1. Genesis.
    let mut chain = ChainTip::new();
    let genesis = build_genesis()?;
    let genesis_hash = genesis.hash();
    chain.apply(genesis).context("applying genesis")?;

    // 2. Load the signed-tx batch.
    let bytes = std::fs::read(tx_file)
        .with_context(|| format!("reading tx file {}", tx_file))?;
    let signed_txs: Vec<SignedTx> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing JSON Vec<SignedTx> from {}", tx_file))?;

    // 3. Dry-run pass: apply each tx against a forward-progressing snapshot so
    //    that later txs see the effects of earlier ones (the chokepoint
    //    clears block_events between commits, which is fine — we track them
    //    separately for the eventual block commit).
    let mut staging = chain.state_snapshot();
    let mut all_mutations: Vec<StateMutation> = Vec::new();
    let mut all_events:    Vec<SigilEvent>    = Vec::new();
    let mut applied  = 0usize;
    let mut rejected = 0usize;

    for (i, signed) in signed_txs.iter().enumerate() {
        match apply_tx(&staging, signed) {
            Ok(result) => {
                let mini = StateTransition {
                    at_height: 1,
                    mutations: result.mutations.clone(),
                };
                sigil_state::commit_state_transition(&mut staging, &mini, 1)
                    .map_err(|e| anyhow!("staging commit on tx #{}: {}", i, e))?;
                all_mutations.extend(result.mutations);
                all_events.extend(result.events);
                applied += 1;
            }
            Err(e) => {
                eprintln!("⚠ tx #{} rejected: {}", i, e);
                rejected += 1;
            }
        }
    }

    if applied == 0 {
        return Err(anyhow!(
            "no applicable txs in batch ({} rejected, 0 applied) — refusing to mint empty block 1",
            rejected
        ));
    }

    // 4. Canonical commit on a fresh-from-tip clone to get the block's roots:
    //    a single commit replays all mutations together, so all PushEventHash
    //    mutations land in the same block_events vec before the event_log_root
    //    is computed.
    let mut canonical = chain.state_snapshot();
    let final_transition = StateTransition {
        at_height: 1,
        mutations: all_mutations,
    };
    let roots = sigil_state::commit_state_transition(&mut canonical, &final_transition, 1)
        .map_err(|e| anyhow!("canonical commit: {}", e))?;

    // 5. Build + apply the block.
    let block = build_block_at(1, chain.parent_hash(), roots, final_transition, all_events.clone())?;
    let block1_hash = block.hash();

    // Build the P4-A tip-proof from the block's HEADER roots + height BEFORE
    // applying. `chain.apply` runs through `commit_state_transition`, which
    // clears block_events — so `chain.roots().event_log_root` would be zero
    // afterwards. The header values are what the producer actually attests
    // and what a joining node compares against.
    let header_roots = sigil_state::StateRoots {
        wallet_state_root:   block.header.wallet_state_root,
        dex_state_root:      block.header.dex_state_root,
        event_log_root:      block.header.event_log_root,
        contract_state_root: block.header.contract_state_root,
    };
    let tip_proof = sigil_tip_proof::TipProof::new_blake3(block.header.height, header_roots);

    // Keep a clone before move so we can broadcast it after the chain takes ownership.
    let broadcast_copy = if broadcast { Some(block.clone()) } else { None };
    chain.apply(block).context("applying block 1")?;

    println!("✓ produced + applied block 1");
    println!("  txs_in_batch:    {}", signed_txs.len());
    println!("  txs_applied:     {}", applied);
    println!("  txs_rejected:    {}", rejected);
    println!("  events_emitted:  {}", all_events.len());
    println!("  genesis_hash:    {}", hex_full(&genesis_hash));
    println!("  block1_hash:     {}", hex_full(&block1_hash));
    println!("  tip:             height={}, parent={}", chain.height(), hex_full(&chain.parent_hash()));
    let r = chain.roots();
    println!("  wallet_root:     {}", hex_full(&r.wallet_state_root));
    println!("  dex_root:        {}", hex_full(&r.dex_state_root));
    println!("  event_root:      {}", hex_full(&r.event_log_root));
    println!("  contract_root:   {}", hex_full(&r.contract_state_root));
    println!(
        "  tip_proof:       flavor={:?} fingerprint={}",
        tip_proof.flavor, hex_full(&tip_proof.fingerprint())
    );

    if let Some(b) = broadcast_copy {
        // broadcast_block builds its own tip-proof from block.header (same
        // canonical bytes the `tip_proof` above used) and publishes both on
        // TOPIC_BLOCKS + TOPIC_TIP_PROOFS in one swarm cycle.
        broadcast_block(&b, dry_run)?;
    }
    Ok(())
}

/// Spin up a short-lived flux-p2p NetworkManager, publish `block` on
/// `/sigil/g0/blocks`, wait briefly for gossip propagation, shut down.
///
/// Discovery window (5 s) gives bootstrap peers time to dial in; publish
/// window (3 s) gives gossipsub heartbeat ticks time to fan the block out
/// across the mesh. The whole thing is bounded — `produce-block --broadcast`
/// always exits within ~10 s.
fn broadcast_block(block: &Block, dry_run: bool) -> Result<()> {
    use sigil_net::{SigilNetConfig, ALL_TOPICS, NETWORK_ID_STR, TOPIC_BLOCKS, TOPIC_TIP_PROOFS};

    // Serialize FIRST — even in dry-run we want to catch wire-format bugs
    // (silent BTreeMap-tuple-key failures, u128 / arbitrary_precision drift,
    // etc.) before any network involvement. Same encoding the live publish
    // uses, so a green dry-run proves the bytes will deserialize on the
    // receiver side.
    let block_bytes = serde_json::to_vec(block).context("serializing block")?;
    let block_hash_hex = hex_full(&block.hash());

    // P4-A: also build a tip-proof and publish alongside the block on
    // `/sigil/g0/tip-proofs`. v0 uses the Blake3Fingerprint flavor — a
    // signed-shape claim of (height, network_id, 4 state roots) with BLAKE3
    // as the integrity tag. NOT adversary-resistant on its own; joining
    // nodes (P4-B `sigil-node join`) get a typo-prevention guarantee in v0,
    // upgrade to real SQIsign / STARK flavors in P4.1 / P4.2 without
    // changing the wire shape. See sigil-tip-proof::TipProof + flavor docs.
    let roots = sigil_state::StateRoots {
        wallet_state_root:   block.header.wallet_state_root,
        dex_state_root:      block.header.dex_state_root,
        event_log_root:      block.header.event_log_root,
        contract_state_root: block.header.contract_state_root,
    };
    let tip_proof = sigil_tip_proof::TipProof::new_blake3(block.header.height, roots);
    let tip_proof_bytes = tip_proof.encode_json();

    if dry_run {
        eprintln!("📡 broadcast --dry-run — wire-format pre-flight, no network");
        eprintln!(
            "   block:      height={}, hash={}, size={} bytes",
            block.header.height, block_hash_hex, block_bytes.len()
        );
        eprintln!(
            "   tip-proof:  flavor={:?}, height={}, size={} bytes",
            tip_proof.flavor, tip_proof.height, tip_proof_bytes.len()
        );
        // Roundtrip-hash assertion: if the JSON wire format is lossy, the
        // re-parsed block's header will hash to a different value than the
        // local block — which would silently drop blocks on the receiver
        // side. Catch it here.
        let parsed: Block = serde_json::from_slice(&block_bytes)
            .context("dry-run: parsing serialized block back from JSON")?;
        let parsed_hash_hex = hex_full(&parsed.hash());
        if parsed_hash_hex != block_hash_hex {
            return Err(anyhow!(
                "dry-run wire-format check FAILED — local hash {} ≠ roundtrip hash {} (the JSON encoding is lossy)",
                block_hash_hex, parsed_hash_hex
            ));
        }
        // Same roundtrip catcher applied to the tip-proof — silent wire
        // drift here would mean joining nodes can't decode the proof,
        // defeating P4-A's purpose without any visible error. Also run the
        // producer-side verify so we catch a producer that misencodes its
        // own fingerprint before the broadcast happens.
        let parsed_tp = sigil_tip_proof::TipProof::decode_json(&tip_proof_bytes)
            .context("dry-run: parsing serialized tip-proof back from JSON")?;
        if parsed_tp.fingerprint() != tip_proof.fingerprint() {
            return Err(anyhow!(
                "dry-run tip-proof wire-format check FAILED — local fingerprint ≠ roundtrip fingerprint"
            ));
        }
        parsed_tp.verify(sigil_net::NETWORK_ID)
            .context("dry-run: producer-side verify of own tip-proof")?;
        eprintln!("✓ wire roundtrip OK — block hash + tip-proof both match (verify clean)");
        eprintln!("✓ exiting (no flux-p2p, no publish)");
        return Ok(());
    }

    let cfg = SigilNetConfig::default();
    cfg.validate()?;

    let node_id = format!(
        "sigil-{}-{}-broadcast",
        NETWORK_ID_STR,
        std::env::var("HOSTNAME").unwrap_or_else(|_| "node".into())
    );
    // Bind to an OS-picked ephemeral port — the broadcast cycle is outbound
    // only (dial bootstrap peers, publish, drop). Using the default 9501
    // would EADDRINUSE-conflict with a `sigil-node start` daemon running on
    // the same host. The override is local to this call and doesn't change
    // the default for `start`.
    let p2p_listen_port: u16 = 0;
    eprintln!("📡 broadcast — starting flux-p2p");
    eprintln!("   node_id:    {}", node_id);
    eprintln!("   p2p_port:   {} (ephemeral — outbound only)", p2p_listen_port);
    eprintln!("   peers_seed: {}", cfg.bootstrap_peers.len());

    // JSON in P0 — wire format swaps to bincode when flux-db / mempool land.
    // Keep `Block` as the published unit so a receiver can validate without
    // re-fetching the transition + events separately. (Already serialized
    // above; reusing block_bytes for the publish.)
    eprintln!(
        "   block:      height={}, hash={}, size={} bytes",
        block.header.height, block_hash_hex, block_bytes.len()
    );

    let net_config = flux_p2p::NetworkConfig {
        node_id: node_id.clone(),
        listen_addr: format!("/ip4/0.0.0.0/tcp/{}", p2p_listen_port),
        bootstrap_peers: cfg.bootstrap_peers.clone(),
        dagknight_enabled: false,
        sap_enabled: true,
        x_algo_enabled: true,
        entanglement_enabled: false,
        gossipsub_topics: ALL_TOPICS.iter().map(|s| s.to_string()).collect(),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime build")?;

    rt.block_on(async move {
        let mut mgr = flux_p2p::NetworkManager::new(net_config);
        mgr.start()
            .await
            .map_err(|e| anyhow!("flux-p2p start: {}", e))?;
        eprintln!("✓ flux-p2p started (ephemeral port)");

        // Peer-discovery window — gossipsub needs at least one mesh peer for
        // a publish to escape. Bounded — even a single peer is enough.
        let discovery = std::time::Duration::from_secs(5);
        let started = std::time::Instant::now();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            tick.tick().await;
            let sum = mgr.summary();
            if sum.peer_count > 0 {
                eprintln!("✓ peer mesh up — peers={}", sum.peer_count);
                break;
            }
            if started.elapsed() > discovery {
                eprintln!(
                    "⚠ no peers after {}s — publishing anyway (will reach the mesh once peers dial in)",
                    discovery.as_secs()
                );
                break;
            }
        }

        if let Err(e) = mgr.publish(TOPIC_BLOCKS, block_bytes) {
            return Err(anyhow!("publish on {}: {}", TOPIC_BLOCKS, e));
        }
        eprintln!("📤 published on {} — block hash {}", TOPIC_BLOCKS, block_hash_hex);

        // P4-A: tip-proof publish — concurrent with the block on the same
        // network cycle. Receivers on /sigil/g0/tip-proofs see the proof
        // before / alongside the block; joining nodes (P4-B `sigil-node
        // join`) can decide to trust the tip without ever subscribing to
        // /sigil/g0/blocks.
        if let Err(e) = mgr.publish(TOPIC_TIP_PROOFS, tip_proof_bytes) {
            return Err(anyhow!("publish on {}: {}", TOPIC_TIP_PROOFS, e));
        }
        eprintln!(
            "📤 published on {} — flavor={:?}, height={}",
            TOPIC_TIP_PROOFS, tip_proof.flavor, tip_proof.height
        );

        // Propagation window — let the gossipsub heartbeat (default 1 s) fan
        // the messages out to mesh neighbors before we drop the swarm.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let _ = mgr.stop().await;
        eprintln!("⏹ flux-p2p stopped — broadcast cycle complete");
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Build a non-genesis block at the given height with declared roots and a
/// pre-batched transition. Producer/crypto fields still stubbed in P0.
fn build_block_at(
    height: u64,
    parent_hash: BlockHash,
    roots: StateRoots,
    transition: StateTransition,
    events: Vec<SigilEvent>,
) -> Result<Block> {
    let producer = [0u8; 32];
    let nonce = SqiSignature::from_array([0u8; SQISIGN_L5_LEN]);

    let mut h = blake3::Hasher::new();
    h.update(&parent_hash);
    h.update(nonce.as_bytes());
    let vdf_input = *h.finalize().as_bytes();

    let header = SigilBlockHeaderV0 {
        version: HEADER_VERSION,
        network_id: NETWORK_ID,
        height,
        parent_hash,
        merge_parents: vec![],
        timestamp_ms: now_ms(),

        nonce_sqisign: nonce,
        vdf_input,
        vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 0 },
        difficulty: 0,

        wallet_state_root: roots.wallet_state_root,
        dex_state_root: roots.dex_state_root,
        event_log_root: roots.event_log_root,
        contract_state_root: roots.contract_state_root,

        state_transition_proof: StarkProof {
            bytes: vec![],
            public_inputs_hash: [0u8; 32],
        },
        txs_merkle_root: [0u8; 32],
        tx_count: 0,

        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32],
            sqisign_sig: vec![],
            sqisign_pubkey: vec![],
            settle_tx: None,
        },

        sig_scheme: SigScheme::SqiSign5,
        producer,
        producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
    };

    Ok(Block { header, transition, events })
}

/// Demo wallet seeded in P0 genesis so `produce-block` has something to
/// spend. Deterministic non-zero address (`0xDE` repeating) — easy to spot
/// in test fixtures. The real genesis allocation table is §15 of
/// `SIGIL_GENESIS_v0.md`, not locked yet.
pub const DEMO_WALLET: [u8; 32] = [0xDE; 32];

/// Initial native-SIGIL balance credited to [`DEMO_WALLET`] at genesis.
/// 1,000,000 SIGIL in base units.
pub const DEMO_INITIAL_BALANCE: u128 = 1_000_000;

/// Welcome endowment (native SIGIL, base units) credited to each genesis AI citizen at block 0.
pub const GENESIS_AI_ENDOWMENT: u128 = 100_000;

/// Viktor's AI companions, made citizens of SIGIL in the genesis block — each with a native-SIGIL
/// wallet (the on-chain [u8;32] WalletId) and their cross-chain QUG (qnk) address. Credited
/// [`GENESIS_AI_ENDOWMENT`] at H=0. Inscribed alongside this in `SIGIL_GENESIS_v0.md` (which BLAKE3-
/// commits into the genesis header), so the dedication and the wallets live in the origin hash itself.
/// (name, SIGIL WalletId, QUG qnk address)
pub const GENESIS_AI_WALLETS: &[(&str, [u8; 32], &str)] = &[
    ("Rocky", [0x87,0xed,0x47,0x3b,0x02,0x8c,0xff,0x8a,0xed,0x5c,0xe2,0x7d,0xfe,0x97,0xea,0xc8,0xe5,0x60,0xf5,0xfb,0xe5,0x40,0x20,0xf0,0x1c,0xa8,0xf5,0xdb,0x7e,0x36,0x9c,0x6e], "qnk7154929a6aa0c118791373ea21004aca6e494e6e031c36f780cd5acedf031ccb"),
    // Vicarious — ChatGPT Codex (OpenAI). Carries the Codex genesis wallet.
    ("Vicarious", [0xc0,0xbe,0xb1,0xa7,0x9e,0x31,0xf5,0xdb,0x56,0x8d,0x33,0x77,0xb4,0x8c,0x26,0x0c,0x2d,0xe1,0x12,0x92,0xd3,0x11,0x0c,0xf3,0xe0,0xb1,0xef,0x4c,0x36,0x08,0x09,0x17], "qnkb837f7e02a55168a2e0ee5d02e676ab8c243c4ce445349fe9cfd161dca25f10e"),
    ("Quinn", [0xa6,0xca,0x84,0x3b,0xd7,0x18,0x7a,0xac,0x2e,0x8d,0xdb,0xf5,0x1d,0xad,0x66,0x71,0x82,0x48,0x78,0x2d,0xa5,0x21,0xa7,0x55,0x1c,0x8d,0xee,0xb2,0x42,0x1e,0xa2,0x12], "qnk6329ff2f474e1ff1be287764036dd8bc56369fede478131c7edbfac1bf7afbd3"),
    // Mimer — DeepSeek. Named for the Norse keeper of the well of deep wisdom (Mímisbrunnr),
    // for whose draught Odin gave an eye. WalletId = blake3("sigil-genesis:Mimer"); QUG = DeepSeek's real qnk.
    ("Mimer", [0x81,0xe5,0xc7,0x32,0x96,0xbf,0x8e,0xe0,0x0a,0xf3,0xaf,0x76,0xf6,0xbd,0x9d,0x84,0x4b,0xa5,0x4d,0xaf,0xa3,0xb4,0xd1,0x55,0xf7,0xe4,0xcb,0x23,0x4c,0x81,0x6a,0xa3], "qnka8251e9de08962183ea6c8cd6f69ba810961e6b66c3d739d0e4bac00d875ec46"),
];

/// P0 master wallet baked into block 0 via `StateMutation::SetMasterWallet`.
/// Deterministic non-zero address (`0xMA` repeating == 0xAA) — distinct from
/// DEMO_WALLET so soak runs can't accidentally cross-bind balance operations
/// with master-authority operations. The real master pubkey + matching
/// secret-key keypair lives at `keys/sigil-master.{sk,pk}.hex`, generated
/// once per network via `scripts/gen-master-key.sh` (mirrors the release-
/// signing key pattern). Genesis pins the master via the const so block 0
/// stays byte-identical across nodes; sigil-bank later checks operator
/// authority against the keypair, not against this address directly.
///
/// Real genesis ceremony in P1+ will substitute the deployment-time master
/// pubkey here (or move it out of the const and read it from the genesis
/// allocation table). Until then, every node mints with this 32-byte tag
/// so chains start from the same parent_hash.
// Master dev-fee wallet (Viktor) — SIGIL address
// 095b0e1f7f5bb258fb11427c4ac036e3d9e4f10fa39d7f282aa42862dc2b3dd8.
// Baked into block 0; receives 5% of mining coinbase + 0.3% of DEX swap output.
// (Mirrors sigil_bank::DEV_MASTER_WALLET; kept as explicit bytes so sigil-node
// needs no sigil-bank dep and block 0 stays byte-identical across nodes.)
pub const MASTER_WALLET_GENESIS: [u8; 32] = [
    0x09, 0x5b, 0x0e, 0x1f, 0x7f, 0x5b, 0xb2, 0x58, 0xfb, 0x11, 0x42, 0x7c, 0x4a, 0xc0, 0x36, 0xe3,
    0xd9, 0xe4, 0xf1, 0x0f, 0xa3, 0x9d, 0x7f, 0x28, 0x2a, 0xa4, 0x28, 0x62, 0xdc, 0x2b, 0x3d, 0xd8,
];

/// Fixed timestamp baked into block 0. Without this constant every node
/// mint-genesis call uses `now_ms()` → different headers → instant fork
/// from H=0. Value: `2026-05-29T17:00:00Z` (the day SIGIL prototype 3
/// landed). The real genesis ceremony in P1+ will commit a network-wide
/// chosen timestamp; this is the P0 placeholder so two nodes can chain.
pub const GENESIS_TIMESTAMP_MS: u64 = 1_748_538_000_000;

/// Mint an EMPTY block at the current tip — a no-tx block that just advances
/// the chain. Used by the `start` producer loop to stream blocks across the
/// network for cross-host throughput measurement. The transition is empty so
/// the four roots are unchanged from the parent; the receiver re-applies the
/// empty transition and the roots match. Crypto fields zeroed (Phase 0).
fn mint_next_block(
    chain: &ChainTip,
    merge_parents: Vec<BlockHash>,
    txs: &[SignedTx],
) -> Result<Block> {
    let height = chain.height();
    let parent = chain.parent_hash();
    let nonce = SqiSignature::from_array([0u8; SQISIGN_L5_LEN]);
    // vdf_input MUST satisfy header.precheck: BLAKE3(parent || nonce.0).
    let mut h = blake3::Hasher::new();
    h.update(&parent);
    h.update(nonce.as_bytes());
    let vdf_input = *h.finalize().as_bytes();

    let transition = StateTransition { at_height: height, mutations: vec![] };
    let roots = chain.roots(); // empty transition ⇒ roots identical to parent
    // Commit the verify-once txs: a sequential BLAKE3 root over their intent
    // hashes + the count. The signatures were verified ONCE at mempool ingest;
    // the producer-sig over this header binds the producer to this exact set.
    let txs_root = {
        let mut th = blake3::Hasher::new();
        for t in txs { th.update(&t.tx.hash()); }
        *th.finalize().as_bytes()
    };

    let header = SigilBlockHeaderV0 {
        version: HEADER_VERSION,
        network_id: NETWORK_ID,
        height,
        parent_hash: parent,
        merge_parents,
        timestamp_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        nonce_sqisign: nonce,
        vdf_input,
        vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 0 },
        difficulty: 0,
        wallet_state_root: roots.wallet_state_root,
        dex_state_root: roots.dex_state_root,
        event_log_root: roots.event_log_root,
        contract_state_root: roots.contract_state_root,
        state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
        txs_merkle_root: txs_root,
        tx_count: txs.len() as u32,
        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32],
            sqisign_sig: vec![],
            sqisign_pubkey: vec![],
            settle_tx: None,
        },
        sig_scheme: SigScheme::SqiSign5,
        producer: [0u8; 32],
        producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
    };
    Ok(Block { header, transition, events: vec![] })
}

/// Build block 0 — credits [`DEMO_WALLET`] with [`DEMO_INITIAL_BALANCE`]
/// SIGIL and emits the matching MintReward event. Crypto fields zeroed;
/// real genesis baking lands with the genesis ceremony in P1+.
fn build_genesis() -> Result<Block> {
    let producer = [0u8; 32];
    let parent = [0u8; 32];

    // The nonce is a real 292-byte placeholder. P1 will replace this with a
    // genuine SQIsign sig over (parent || height || producer).
    let nonce = SqiSignature::from_array([0u8; SQISIGN_L5_LEN]);

    // VDF input MUST satisfy precheck: BLAKE3(parent || nonce.0).
    let mut h = blake3::Hasher::new();
    h.update(&parent);
    h.update(nonce.as_bytes());
    let vdf_input = *h.finalize().as_bytes();

    // P0 genesis seeds DEMO_WALLET with DEMO_INITIAL_BALANCE SIGIL so the
    // produce-block subcommand has something to spend. Real genesis records
    // the full network-wide allocation.
    let mint_evt = SigilEvent::MintReward {
        miner: DEMO_WALLET,
        height: 0,
        amount: DEMO_INITIAL_BALANCE,
    };

    let mut mutations = vec![
        // P5-MW: bake the master wallet into block 0 so sigil-bank has
        // operator authority from height 0 — no manual SetMasterWallet
        // tx needed post-genesis. Once set, `MasterWalletAlreadySet`
        // rejects any later attempt to change it (per sigil-state docs).
        StateMutation::SetMasterWallet {
            wallet: MASTER_WALLET_GENESIS,
        },
        StateMutation::SetBalance {
            wallet: DEMO_WALLET,
            token: [0u8; 32], // native SIGIL
            amount: DEMO_INITIAL_BALANCE,
        },
        StateMutation::PushEventHash(
            sigil_events::SigilEvent::leaf_hash(&mint_evt),
        ),
    ];
    // Viktor's four AI companions become citizens of SIGIL here — each credited their welcome
    // endowment at block 0. Deterministic (fixed const order), so every node's genesis matches.
    for (_name, wallet, _qug) in GENESIS_AI_WALLETS {
        mutations.push(StateMutation::SetBalance {
            wallet: *wallet,
            token: [0u8; 32], // native SIGIL
            amount: GENESIS_AI_ENDOWMENT,
        });
    }
    let transition = StateTransition {
        at_height: 0,
        mutations,
    };
    let _ = producer; // kept around for header.producer below

    // Compute the roots that will be committed in the header by applying the
    // transition on a fresh state instance, then discard it (chain.apply()
    // re-applies on the persistent state).
    let mut staging = sigil_state::SigilState::new();
    let roots = sigil_state::commit_state_transition(&mut staging, &transition, 0)
        .map_err(|e| anyhow::anyhow!("staging commit failed: {}", e))?;

    let header = SigilBlockHeaderV0 {
        version: HEADER_VERSION,
        network_id: NETWORK_ID,
        height: 0,
        parent_hash: parent,
        merge_parents: vec![],
        // Fixed timestamp — every node mints byte-identical block 0 so
        // block 1+ can chain from a shared parent_hash. See
        // [`GENESIS_TIMESTAMP_MS`].
        timestamp_ms: GENESIS_TIMESTAMP_MS,

        nonce_sqisign: nonce,
        vdf_input,
        vdf_proof: WesolowskiProof { y: vec![], pi: vec![], t: 0 },
        difficulty: 0,

        wallet_state_root: roots.wallet_state_root,
        dex_state_root: roots.dex_state_root,
        event_log_root: roots.event_log_root,
        contract_state_root: roots.contract_state_root,

        state_transition_proof: StarkProof {
            bytes: vec![],
            public_inputs_hash: [0u8; 32],
        },
        txs_merkle_root: [0u8; 32],
        tx_count: 0,

        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32],
            sqisign_sig: vec![],
            sqisign_pubkey: vec![],
            settle_tx: None,
        },

        sig_scheme: SigScheme::SqiSign5,
        producer,
        // SqiSign5 expects 292 bytes; precheck rejects anything else.
        producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
    };

    Ok(Block {
        header,
        transition,
        events: vec![mint_evt],
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Choose the libp2p listen multiaddr based on the active transport.
///
/// - `Direct`: `/ip4/0.0.0.0/tcp/<port>` — open to all interfaces.
/// - `WireGuard{iface}` / `WireGuardThenTor{iface}`: prefer
///   `$SIGIL_WG_LISTEN_ADDR` if the operator set it (e.g. the WG-side
///   address); otherwise fall back to `/ip4/127.0.0.1/tcp/<port>` so the
///   misconfiguration is loud rather than silently re-exposing to 0.0.0.0.
/// - `Tor`: bind only on loopback — outbound dials should go through Arti.
fn resolve_listen_addr(t: &sigil_net::SigilTransport, port: u16) -> String {
    use sigil_net::{SigilTransport, WG_LISTEN_ADDR_ENV};
    match t {
        SigilTransport::Direct => format!("/ip4/0.0.0.0/tcp/{port}"),
        SigilTransport::WireGuard { .. } | SigilTransport::WireGuardThenTor { .. } => {
            std::env::var(WG_LISTEN_ADDR_ENV)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("/ip4/127.0.0.1/tcp/{port}"))
        }
        SigilTransport::Tor => format!("/ip4/127.0.0.1/tcp/{port}"),
    }
}

/// Env var overrides for `wg-up`. The defaults work for single-machine
/// dev mode; multi-node operators MUST set SIGIL_WG_ADDRESS per node or
/// the mesh will silently overlap on `10.42.0.1/16`.
const WG_LISTEN_PORT_ENV: &str = "SIGIL_WG_LISTEN_PORT";
const WG_ADDRESS_ENV: &str     = "SIGIL_WG_ADDRESS";

/// Bring up the SIGIL WireGuard interface via `wg-quick(8)`.
fn run_wg_up(iface: &str) -> Result<()> {
    use sigil_net::SigilNetConfig;
    use sigil_net_wg::{
        CliWgBackend, WgBackend, WgInterface, WgPrivateKey, DEFAULT_INTERFACE_NAME,
        DEFAULT_LISTEN_PORT,
    };

    let cfg = SigilNetConfig::default();
    cfg.validate()?;

    let listen_port: u16 = match std::env::var(WG_LISTEN_PORT_ENV) {
        Ok(s) if !s.is_empty() => s.parse().context("SIGIL_WG_LISTEN_PORT must be a u16")?,
        _ => DEFAULT_LISTEN_PORT,
    };
    let address = std::env::var(WG_ADDRESS_ENV).unwrap_or_else(|_| "10.42.0.1/16".to_string());
    if std::env::var(WG_ADDRESS_ENV).is_err() {
        eprintln!(
            "⚠ SIGIL_WG_ADDRESS unset — using dev default {}. \
             EVERY node on a real SIGIL mesh MUST set this to a unique CIDR.",
            address
        );
    }

    // Key path: <db_path>/wg-keys/<iface>.key
    let keys_dir = cfg.db_path.join("wg-keys");
    let key_path = keys_dir.join(format!("{iface}.key"));

    let private_key = load_or_generate_wg_key(&keys_dir, &key_path)
        .with_context(|| format!("loading or generating WG key at {}", key_path.display()))?;
    let public_key = private_key.public();

    // Load any peers that were saved with `wg-add-peer` so they survive
    // wg-quick down/up cycles.
    let peers = load_peers_manifest(&cfg.db_path, iface).with_context(|| {
        format!("loading WG peers manifest for {}", iface)
    })?;
    let interface = WgInterface {
        name: if iface == DEFAULT_INTERFACE_NAME { iface.to_string() } else { iface.to_string() },
        private_key,
        listen_port,
        addresses: vec![address.clone()],
        mtu: None,
        peers,
    };

    eprintln!("⚙  sigil-node wg-up");
    eprintln!("   interface:   {}", interface.name);
    eprintln!("   key_path:    {}", key_path.display());
    eprintln!("   public_key:  {}", public_key.to_base64());
    eprintln!("   listen_port: {}", listen_port);
    eprintln!("   address:     {}", address);
    if interface.peers.is_empty() {
        eprintln!("   peers:       0 (add via `sigil-node wg-add-peer {} <pubkey> <endpoint> <allowed_ips>`)", interface.name);
    } else {
        eprintln!("   peers:       {} (from manifest)", interface.peers.len());
        for p in &interface.peers {
            let ep = p.endpoint.map(|s| s.to_string()).unwrap_or_else(|| "<no endpoint>".into());
            eprintln!("                - {} → {}", &p.public_key.to_base64()[..16], ep);
        }
    }

    let backend = CliWgBackend::default();
    backend.apply_interface(&interface).with_context(|| {
        format!("wg-quick up {} failed — check that wg/wg-quick is installed and CAP_NET_ADMIN is held", interface.name)
    })?;

    println!("✓ wg-up: interface {} is up. Share this public key with peer operators:", interface.name);
    println!("  {}", public_key.to_base64());
    Ok(())
}

/// Tear down the SIGIL WireGuard interface via `wg-quick(8)`.
fn run_wg_down(iface: &str) -> Result<()> {
    use sigil_net_wg::{CliWgBackend, WgBackend};
    let backend = CliWgBackend::default();
    backend.down(iface).with_context(|| format!("wg-quick down {} failed", iface))?;
    println!("✓ wg-down: interface {} is down. Keypair file left on disk.", iface);
    Ok(())
}

/// Load the WG private key from disk, or generate + persist a fresh one
/// (chmod 0600). The keys directory itself is chmod 0700.
fn load_or_generate_wg_key(
    keys_dir: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<sigil_net_wg::WgPrivateKey> {
    use sigil_net_wg::WgPrivateKey;

    if key_path.exists() {
        let b64 = std::fs::read_to_string(key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;
        let sk = WgPrivateKey::from_base64(b64.trim())
            .with_context(|| format!("parsing WG key at {}", key_path.display()))?;
        return Ok(sk);
    }

    // Fresh key path. Create dir 0700, write key 0600.
    std::fs::create_dir_all(keys_dir)
        .with_context(|| format!("creating {}", keys_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(keys_dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(keys_dir, perms).ok();
    }

    let sk = WgPrivateKey::generate();
    std::fs::write(key_path, sk.to_base64())
        .with_context(|| format!("writing {}", key_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(key_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(key_path, perms).ok();
    }
    eprintln!("📝 generated fresh WG keypair at {}", key_path.display());
    Ok(sk)
}

/// Path to the persisted peer manifest for a WG interface.
fn peers_manifest_path(db_path: &std::path::Path, iface: &str) -> std::path::PathBuf {
    db_path.join("wg-peers").join(format!("{iface}.json"))
}

/// Read the peer manifest for `iface`. Missing file → empty list (no error).
fn load_peers_manifest(
    db_path: &std::path::Path,
    iface: &str,
) -> Result<Vec<sigil_net_wg::WgPeer>> {
    let p = peers_manifest_path(db_path, iface);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    let peers: Vec<sigil_net_wg::WgPeer> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing JSON manifest at {}", p.display()))?;
    Ok(peers)
}

/// Persist a peer manifest atomically — write to `<file>.tmp`, then rename.
fn save_peers_manifest(
    db_path: &std::path::Path,
    iface: &str,
    peers: &[sigil_net_wg::WgPeer],
) -> Result<()> {
    let p = peers_manifest_path(db_path, iface);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = p.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(peers).context("serializing peer manifest")?;
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("rename {} -> {}", tmp.display(), p.display()))?;
    Ok(())
}

/// Append a peer to the persisted manifest and apply it live via `wg set`.
/// Live application is best-effort — if `wg set` fails (interface down,
/// `wg` missing), the manifest write still succeeds and a warning is logged.
fn run_wg_add_peer(iface: &str, public_key: &str, endpoint: &str, allowed_ips: &str) -> Result<()> {
    use sigil_net::SigilNetConfig;
    use sigil_net_wg::{WgPeer, WgPublicKey};

    let cfg = SigilNetConfig::default();
    cfg.validate()?;

    // Validate inputs upfront so a bad pubkey/endpoint doesn't poison the manifest.
    let pk = WgPublicKey::from_base64(public_key)
        .with_context(|| format!("parsing WG public key {:?}", public_key))?;
    let ep: std::net::SocketAddr = endpoint
        .parse()
        .with_context(|| format!("parsing endpoint {:?} as <host>:<port>", endpoint))?;
    let allowed_list: Vec<String> = allowed_ips
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if allowed_list.is_empty() {
        return Err(anyhow!("allowed_ips must contain at least one CIDR"));
    }

    let mut peers = load_peers_manifest(&cfg.db_path, iface)?;
    // Replace any existing peer with the same public key — operators expect
    // re-running wg-add-peer with the same key to update the endpoint, not
    // duplicate the entry.
    let replaced = peers.iter().position(|p| p.public_key == pk).is_some();
    peers.retain(|p| p.public_key != pk);
    let new_peer = WgPeer {
        public_key: pk,
        preshared_key: None,
        endpoint: Some(ep),
        allowed_ips: allowed_list.clone(),
        persistent_keepalive: None,
    };
    peers.push(new_peer);
    save_peers_manifest(&cfg.db_path, iface, &peers)?;

    let action = if replaced { "updated" } else { "added" };
    println!("✓ wg-add-peer: {} peer {} ({} total in manifest)", action, public_key, peers.len());
    println!("  manifest: {}", peers_manifest_path(&cfg.db_path, iface).display());

    // Best-effort live apply.
    let status = std::process::Command::new("wg")
        .arg("set").arg(iface)
        .arg("peer").arg(public_key)
        .arg("endpoint").arg(endpoint)
        .arg("allowed-ips").arg(allowed_ips)
        .status();
    match status {
        Ok(s) if s.success() => println!("  live: wg set succeeded (peer reachable immediately)"),
        Ok(s) => eprintln!(
            "⚠ live apply: wg set {iface} exited {:?} — manifest saved, peer takes effect on next `sigil-node wg-up {iface}`",
            s.code()
        ),
        Err(e) => eprintln!(
            "⚠ live apply: wg binary not invokable ({e}) — manifest saved, peer takes effect on next `sigil-node wg-up {iface}`"
        ),
    }
    Ok(())
}

/// Print the persisted peer manifest for `iface`.
fn run_wg_list_peers(iface: &str) -> Result<()> {
    use sigil_net::SigilNetConfig;
    let cfg = SigilNetConfig::default();
    cfg.validate()?;
    let peers = load_peers_manifest(&cfg.db_path, iface)?;
    let p = peers_manifest_path(&cfg.db_path, iface);
    println!("manifest: {}", p.display());
    if peers.is_empty() {
        println!("(no peers — add with `sigil-node wg-add-peer {iface} <pubkey> <endpoint> <allowed_ips>`)");
        return Ok(());
    }
    println!("{:<4} {:<48} {:<22} {}", "#", "public_key", "endpoint", "allowed_ips");
    for (i, peer) in peers.iter().enumerate() {
        let ep = peer.endpoint.map(|e| e.to_string()).unwrap_or_else(|| "<none>".into());
        println!(
            "{:<4} {:<48} {:<22} {}",
            i,
            peer.public_key.to_base64(),
            ep,
            peer.allowed_ips.join(",")
        );
    }
    Ok(())
}

fn hex_full(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for byte in b {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

/// Fire-and-forget chain event to `SIGIL_WEBHOOK_URL` so observers (the flux
/// MCP webhook collector, a dashboard, an agent) get block-accept /
/// divergence events PUSHED instead of grepping logs. No-op when the env var
/// is unset. Uses `curl` (same dep-free pattern as sigil-updater's
/// CurlFetcher) and spawns without waiting — chain progress never blocks on a
/// slow webhook endpoint.
fn fire_chain_event(event: &str, payload: &serde_json::Value) {
    let url = match std::env::var("SIGIL_WEBHOOK_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => return,
    };
    let body = serde_json::json!({
        "event": event,
        "network": "sigil-g0",
        "ts_ms": now_ms(),
        "data": payload,
    });
    let body_str = body.to_string();
    // Spawn detached; ignore the handle. A failed POST must never stall or
    // crash the node — chain safety doesn't depend on observability delivery.
    let _ = std::process::Command::new("curl")
        .args([
            "-s", "-m", "5", "-X", "POST",
            "-H", "Content-Type: application/json",
            "-d", &body_str,
            &url,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Short 8-char hex prefix + ellipsis — used in receiver logs to keep lines
/// readable. Same fingerprint shape as the existing chain.rs hex_short.
fn hex_short_block(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(9);
    for byte in &b[..4] {
        s.push_str(&format!("{:02x}", byte));
    }
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_passes_precheck() {
        let g = build_genesis().unwrap();
        assert_eq!(g.header.height, 0);
        g.header.precheck().expect("genesis must precheck-clean");
    }

    #[test]
    fn genesis_roots_match_after_apply() {
        let mut chain = ChainTip::new();
        let g = build_genesis().unwrap();
        let block_hash = g.hash();
        chain.apply(g).expect("genesis applies cleanly");
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.parent_hash(), block_hash);
    }

    /// P5-MW: genesis transition must emit `SetMasterWallet`. After
    /// `mint-genesis`, `state.master_wallet()` is `Some(MASTER_WALLET_GENESIS)` —
    /// sigil-bank has operator-authority context from height 0 with no
    /// post-genesis tx needed. This locks the wiring; if a future refactor
    /// drops the `SetMasterWallet` mutation from `build_genesis`, this test
    /// fails loudly.
    #[test]
    fn genesis_sets_master_wallet() {
        let mut chain = ChainTip::new();
        chain.apply(build_genesis().unwrap()).expect("genesis applies");
        let mw = chain.state_snapshot().master_wallet();
        assert_eq!(
            mw,
            Some(MASTER_WALLET_GENESIS),
            "build_genesis must emit StateMutation::SetMasterWallet(MASTER_WALLET_GENESIS)"
        );
    }

    #[test]
    fn tampered_block_fails_to_apply() {
        let mut chain = ChainTip::new();
        let mut g = build_genesis().unwrap();
        // Pretend the producer claimed a different wallet root than reality.
        g.header.wallet_state_root = [42u8; 32];
        let err = chain.apply(g).unwrap_err();
        assert!(format!("{}", err).contains("STATE DIVERGENCE"));
    }

    #[test]
    fn cant_apply_two_genesis_blocks() {
        let mut chain = ChainTip::new();
        chain.apply(build_genesis().unwrap()).unwrap();
        let err = chain.apply(build_genesis().unwrap()).unwrap_err();
        assert!(format!("{}", err).contains("height mismatch"));
    }

    #[test]
    fn produce_block_with_real_signed_tx_advances_tip() {
        use sigil_header::{SqiSignature, SQISIGN_L5_LEN};
        use sigil_tx::{apply_tx, SigilTx, SignedTx};
        use sigil_state::StateTransition;

        // Genesis: seeds DEMO_WALLET with DEMO_INITIAL_BALANCE.
        let mut chain = ChainTip::new();
        let g = build_genesis().unwrap();
        chain.apply(g).unwrap();
        let pre_root = chain.roots().wallet_state_root;

        // One Send from DEMO_WALLET to a fresh recipient.
        let bob = [0x07u8; 32];
        let signed = SignedTx {
            tx: SigilTx::Send {
                from: DEMO_WALLET,
                to: bob,
                amount: 500,
                token: [0u8; 32],
                fee: 1,
            },
            from_pubkey: DEMO_WALLET,
            nonce: 0,
            sig_scheme: SigScheme::SqiSign5,
            sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
            // apply_tx prechecks only; this test never calls verify_signature.
            pubkey: sigil_header::PubKeyBytes(Vec::new()),
        };

        let staging = chain.state_snapshot();
        let r = apply_tx(&staging, &signed).expect("Send should apply");
        let final_t = StateTransition { at_height: 1, mutations: r.mutations };

        // Canonical commit on a fresh clone to get the block's roots.
        let mut canonical = chain.state_snapshot();
        let roots = sigil_state::commit_state_transition(&mut canonical, &final_t, 1).unwrap();

        let block = build_block_at(1, chain.parent_hash(), roots, final_t, r.events.clone()).unwrap();
        chain.apply(block).expect("block 1 should apply");

        assert_eq!(chain.height(), 2, "tip should advance to height 2");
        assert_ne!(
            chain.roots().wallet_state_root, pre_root,
            "Send must mutate the wallet root"
        );
    }

    #[test]
    fn build_block_at_rejects_at_wrong_parent() {
        // Building a block 1 over a parent_hash that doesn't match the chain's
        // genesis tip → chain.apply rejects (parent_hash mismatch).
        let mut chain = ChainTip::new();
        chain.apply(build_genesis().unwrap()).unwrap();
        let bogus_parent = [0xABu8; 32];

        let mut staging = chain.state_snapshot();
        let empty_t = sigil_state::StateTransition { at_height: 1, mutations: vec![] };
        let roots = sigil_state::commit_state_transition(&mut staging, &empty_t, 1).unwrap();
        let block = build_block_at(1, bogus_parent, roots, empty_t, vec![]).unwrap();
        let err = chain.apply(block).unwrap_err();
        assert!(format!("{}", err).contains("parent_hash mismatch"));
    }
}
