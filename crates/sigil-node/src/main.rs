//! sigil-node — SIGIL block producer + verifier binary.
//!
//! Phase 0 wires together Track C (header + state + events) into a runnable
//! binary that proves the type composition works end-to-end. No networking,
//! no consensus, no real crypto: those crates land in P1+. The point of P0
//! is to be able to say `sigil-node mint-genesis` on Delta or Epsilon and
//! get a well-formed block 0 with all four roots computed locally.

mod block;
mod chain;
mod coinbase; // ONE-CHAIN step 1: braid mints REAL coinbase blocks (money enters the graph)
mod chain_log;
mod cli;
mod snapshot;
mod rate_governor;
mod ingest;
mod sync_auth;

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use sigil_dagknight::{BlockView, Braid, BraidConfig, InsertOutcome};
use sigil_events::SigilEvent;
use sigil_header::{
    BlockHash, ProofBundle, SigScheme, SigilBlockHeaderV0, SignatureBytes, SqiSignature,
    StarkProof, WesolowskiProof, HEADER_VERSION, NETWORK_ID, SQISIGN_L5_LEN,
};
use sigil_state::{SigilState, StateMutation, StateRoots, StateTransition, WalletId};
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
    /// H2 verify-before-sync: the requester's signed session handshake
    /// (ed25519 over the transcript, `session_pubkey` = requester's libp2p
    /// peer-id string for channel binding). Old servers ignore the unknown
    /// field; old clients omit it → `None`. See `sync_auth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handshake: Option<sigil_handshake::EphemeralSessionHandshakeV0>,
}

/// Point-to-point backfill response: the requested block range serialized as
/// JSON values (each element = `serde_json::to_value(&Block)`).
#[derive(serde::Serialize, serde::Deserialize)]
struct BackfillResp {
    blocks: Vec<crate::block::Block>,
}

// ── codec=2 SNAPSHOT WIRE — server side (LANE-B, rocky-sync-B; v3 sync sprint) ──────────
// Server emitters for A's frozen codec=2 design (docs/SIGIL_SKELETON_CODEC2_v0.md):
//   codec=3 → 'P' + bincode(SnapshotHeader)   (discovery/framing; tip-dependent)
//   codec=2 → 'S' + bincode(Vec<SkeletonRecord>)  (72 B/record skeleton page)
//   codec=4 → 'F' + bincode(SnapshotTrailer)   (DEFERRED — needs flux-fold dep + a
//             producer SQIsign over (archive_root‖anchor_height‖anchor_hash); until then
//             we serve nothing for codec=4 so the client benches+downgrades → ZERO regression)
// These MUST stay byte-identical to the client copies in sigil-top/block_sync/fetch.rs
// (bincode keys on field order+type, not the struct's name/crate). Canonical home is a
// shared crate (sigil-header) — tracked with A; duplicated here so the server compiles in
// isolation. The 4 state roots are intentionally ABSENT (B #416, DeepSeek-verified: a flat
// fold can't bind interior roots; trusted roots come only from frontier full headers or the
// signed anchor).
const SNAPSHOT_MAGIC: [u8; 4] = *b"SGSN";
const SNAPSHOT_VERSION: u16 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct SkeletonRecord {
    height: u64,
    block_hash: [u8; 32],
    parent_hash: [u8; 32],
}
impl SkeletonRecord {
    fn from_header(h: &sigil_header::SigilBlockHeaderV0) -> Self {
        Self { height: h.height, block_hash: h.hash(), parent_hash: h.parent_hash }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotHeader {
    magic: [u8; 4],
    version: u16,
    base_height: u64,
    anchor_height: u64,
    anchor_hash: [u8; 32],
    count: u64,
}

#[cfg(test)]
mod snapshot_wire_tests {
    use super::{SkeletonRecord, SnapshotHeader, SNAPSHOT_MAGIC, SNAPSHOT_VERSION};

    /// THE cross-crate compat invariant: a SkeletonRecord MUST be exactly 72 B under
    /// bincode (u64 + 2×[u8;32], no length prefixes) — byte-identical to the client's
    /// assertion in sigil-top/block_sync/fetch.rs. If this drifts, the wire breaks.
    #[test]
    fn skeleton_record_is_72_bytes_on_the_wire() {
        let rec = SkeletonRecord { height: 7, block_hash: [1u8; 32], parent_hash: [2u8; 32] };
        assert_eq!(bincode::serialize(&rec).unwrap().len(), 72);
    }

    /// SnapshotHeader round-trips bincode (what the client's `bincode::deserialize::<SnapshotHeader>`
    /// consumes after the 'P' tag) and carries the frozen magic/version.
    #[test]
    fn snapshot_header_roundtrips() {
        let h = SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            version: SNAPSHOT_VERSION,
            base_height: 0,
            anchor_height: 128_000_000,
            anchor_hash: [9u8; 32],
            count: 128_000_001,
        };
        let bytes = bincode::serialize(&h).unwrap();
        let back: SnapshotHeader = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.magic, *b"SGSN");
        assert_eq!(back.version, 1);
        assert_eq!(back.anchor_height, 128_000_000);
        assert_eq!(back.count, 128_000_001);
    }
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

    // ── H2 verify-before-sync: node identity + signed handshake + serve gate ──
    // One 12h ValidatorPeer handshake minted at startup, attached to every
    // outgoing BackfillReq; the serve path admits peers via `sync_auth_gate`
    // (log-only unless SIGIL_HANDSHAKE_REQUIRE=1). Wire-compatible both ways.
    let hs_sk = sync_auth::load_or_create_identity(&cfg.db_path);
    let sync_hs = std::sync::Arc::new(sync_auth::mint(&hs_sk, NETWORK_ID_STR, &local_peer_id, now_ms()));
    let mut sync_auth_gate = sync_auth::SyncAuth::from_env(NETWORK_ID_STR);
    eprintln!(
        "   sync-auth:       H2 handshake minted (ValidatorPeer, 12h) · enforce={}",
        sync_auth_gate.enforcing()
    );

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
        // ONE-CHAIN: the adaptive emission controller (opt-in SIGIL_EMISSION_ADAPTIVE=1).
        // Persisted watermark survives restarts; genesis anchored at first run.
        let emission_genesis_ts = std::env::var("SIGIL_GENESIS_TS").ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
        let mut emission = coinbase::load_controller(&snap_dir, emission_genesis_ts);
        if emission.is_some() {
            eprintln!("💠 adaptive emission controller LIVE — time-based halving + PID rate control (watermark persisted)");
        }
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
        // R1: tx/batch INGEST BRIDGE — the first real user-tx path into the producer
        // mempool (wallets / rpcd-forwarder / loadgen). Env-gated by SIGIL_API_PORT;
        // shares the mempool Arc like the TXGEN feeder. Off unless set.
        if let Some(api_port) = std::env::var("SIGIL_API_PORT").ok()
            .and_then(|s| s.parse::<u16>().ok()).filter(|p| *p > 0)
        {
            crate::ingest::spawn(Arc::clone(&mempool), api_port);
            eprintln!("\u{1f310} tx ingest API on :{api_port} — POST /tx, POST /batch, GET /mempool");
        }

        // ONE-CHAIN step 2: the PROPER money API (sigil-api, axum + flux-api SDKs)
        // — balance / supply / signed-send / tx-status. Shares this producer's
        // mempool + a published state snapshot. Supersedes rpcd; gated by
        // SIGIL_MONEY_API=<addr> (e.g. 0.0.0.0:8181).
        // P1 mining-onto-the-braid: the bridge the producer publishes its frontier
        // into and pops verified dual-lane solves from. Always constructed (cheap,
        // inert without a producer); the API exposes it, and `SIGIL_MINING_GATED=1`
        // makes block production wait on real work.
        let mining_bridge = Arc::new(sigil_api::mining::MiningBridge::new());
        let money_state: Option<Arc<std::sync::RwLock<SigilState>>> =
            std::env::var("SIGIL_MONEY_API").ok().filter(|s| !s.is_empty()).map(|addr| {
                let shared = Arc::new(std::sync::RwLock::new(chain.state_snapshot()));
                let app = sigil_api::AppState {
                    mempool: Arc::clone(&mempool),
                    state: Arc::clone(&shared),
                    mining: Arc::clone(&mining_bridge),
                };
                tokio::spawn(async move {
                    if let Err(e) = sigil_api::serve(&addr, app).await {
                        eprintln!("\u{26a0} sigil-api serve failed: {e}");
                    }
                });
                eprintln!("\u{1f4b0} sigil-api money API on {} — /v1/{{balance,supply,transactions,mining/*}}", std::env::var("SIGIL_MONEY_API").unwrap_or_default());
                shared
            });
        // When gated, a braid block is minted ONLY for a verified dual-lane solve —
        // the braid stops being a free-running dyno and starts costing power+time.
        let mining_gated = std::env::var("SIGIL_MINING_GATED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
        if mining_gated {
            eprintln!("\u{26cf} MINING-GATED production — blocks mint only on a verified \
                dual-lane solve (\u{03a6} BLAKE4 {} bits + \u{03a9} VDF t={})",
                sigil_api::mining::blake4_bits(), sigil_api::mining::vdf_t());
        }
        // ── adaptive block-rate governor (demand-responsive; SIGIL_RATE_ADAPTIVE=1) ──
        // Idle -> SIGIL_RATE_MIN (heartbeat, no wasted empty blocks); mempool backlog
        // raises the rate to bound tx-inclusion latency, clamped to SIGIL_RATE_MAX.
        let mut rate_gov: Option<rate_governor::RateGovernor> = if produce
            && std::env::var("SIGIL_RATE_ADAPTIVE").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
        {
            let cap = if txgen > 0 { txgen as f64 } else { 256.0 };
            let g = rate_governor::RateGovernor::from_env(cap);
            eprintln!("\u{1f39a} ADAPTIVE RATE governor ON — floor {:.0}/s · ceiling {:.0}/s · target latency {:.1}s · cap {:.0} tx/blk",
                g.rate_min, g.rate_max, g.target_latency_s, g.tx_capacity);
            Some(g)
        } else { None };
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
        // SIGIL_DAG=1 REAL ordering: the braid is only constructed in dag_mode —
        // None ⇒ SIGIL_DAG=0 behavior-identical (design §3.1). Seeded from the
        // local chain's in-RAM window so the producer's own spine is known.
        let mut braid: Option<Braid> = dag_mode.then(|| dag_seed_braid(&chain));
        if dag_mode {
            // Honest, ACTUAL-state log line (was a hardcoded "v1, NOT GHOSTDAG"
            // string regardless of config — fixed so it reports what the
            // constructed braid is really running, per the crate's own
            // honest-naming discipline).
            if braid.as_ref().is_some_and(|b| b.is_ghostdag_active()) {
                eprintln!("🕸 DAG mode — v2 GHOSTDAG-style k-cluster blue/red coloring ACTIVE \
                    (SIGIL_DAG_GHOSTDAG_K set; still NOT DagKnight-the-paper — see \
                    sigil-dagknight's ghostdag module doc for exact scope)");
            } else {
                eprintln!("🕸 DAG mode — real braid ordering: deterministic braid linearization v1 \
                    (NOT GHOSTDAG, NOT DagKnight-the-paper — docs/SIGIL_DAGKNIGHT_LANE_v0.md §1)");
            }
        }
        // Full bodies awaiting/holding braid order, RAM-only + bounded (design
        // §3.1): evicted below finalized height after each drain, hard-capped.
        let dag_max_bodies: usize = std::env::var("SIGIL_DAG_MAX_BODIES").ok()
            .and_then(|v| v.trim().parse().ok()).unwrap_or(32_768);
        let mut dag_bodies: std::collections::HashMap<BlockHash, crate::block::Block> =
            std::collections::HashMap::new();
        // v0 metrics: ordered-but-not-state-applied (off-spine / non-extending /
        // already-applied), refused-below-final, structural rejects, apply fails.
        let (mut dag_ord_skipped, mut dag_below_final, mut dag_rejected, mut dag_apply_failed) =
            (0u64, 0u64, 0u64, 0u64);
        // Wedge self-heal markers (see the Rejected arms): reject count and tip
        // height at the last reseed — reseeds are progress-gated on the height.
        let mut dag_last_reseed_rejects: u64 = 0;
        let mut dag_last_reseed_height: u64 = 0;
        // Adaptive governor (if on) starts at its floor; else the fixed produce_us.
        let initial_produce_us = rate_gov.as_ref()
            .map(|g| (1_000_000.0 / g.rate_min) as u64)
            .unwrap_or(produce_us);
        let mut produce_tick =
            tokio::time::interval(std::time::Duration::from_micros(initial_produce_us.max(50)));
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
        let (bf_tx, mut bf_rx) = tokio::sync::mpsc::channel::<Vec<crate::block::Block>>(64);
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
        // === request-ahead fetch pipeline (windowed backfill — overlaps fetch with apply) ===
        let mut req_frontier: u64 = chain.height();
        let mut net_tip: u64 = 0;
        const FETCH_CHUNK: u64 = 8192;
        const FETCH_MAX_AHEAD: u64 = 131_072; // keep ~16 ranges in flight ahead of the applied tip
        loop {
            // Slide the window: fire consecutive range-requests until req_frontier is
            // FETCH_MAX_AHEAD past the applied tip. Height-bounded => apply backpressures fetch.
            // The existing `expected`-based requests below stay as gap-recovery; pending dedups.
            if !produce && !diverged {
                let tip = chain.height();
                if tip > req_frontier { req_frontier = tip; }
                while req_frontier < net_tip && req_frontier < tip + FETCH_MAX_AHEAD {
                    if let Some(peer) = mgr.connected_peers().into_iter().next() {
                        let from = req_frontier;
                        let to = (from + FETCH_CHUNK).min(net_tip);
                        let req = BackfillReq { from, to, headers_only: false, codec: 0, handshake: Some((*sync_hs).clone()) };
                        let mgr2 = std::sync::Arc::clone(&mgr);
                        let bf_tx2 = bf_tx.clone();
                        tokio::spawn(async move {
                            let blocks = if let Ok(payload) = serde_json::to_vec(&req) {
                                match mgr2.send_request(peer, payload).await {
                                    Ok(bytes) => bincode::deserialize::<BackfillResp>(&bytes).map(|r| r.blocks).unwrap_or_default(),
                                    Err(_) => Vec::new(),
                                }
                            } else { Vec::new() };
                            let _ = bf_tx2.send(blocks).await;
                        });
                        req_frontier += FETCH_CHUNK;
                    } else { break; }
                }
            }
            tokio::select! {
                _ = produce_tick.tick(), if produce => {
                    // Gate production on (a) having a peer and (b) a grace
                    // period after the first peer so the gossipsub mesh grafts
                    // — otherwise the receiver joins mid-stream and gaps
                    // forever (Phase 0 has no backfill). Once grace elapses,
                    // both advance from H=1 in lockstep.
                    let peers = mgr.summary().peer_count;
                    // DEV: SIGIL_SOLO_MINT=1 lets a single node mint with no peer
                    // (self-contained coinbase/chronos proofs). Off by default —
                    // the peer+grace gate below is unchanged for real meshes.
                    let solo = std::env::var("SIGIL_SOLO_MINT").map(|v| v == "1").unwrap_or(false);
                    if solo && !producing {
                        producing = true;
                        eprintln!("🏭 SOLO_MINT — dev proof, minting without a peer");
                    } else if peers == 0 && !solo {
                        first_peer_at = None;
                        producing = false;
                    } else if first_peer_at.is_none() && !solo {
                        first_peer_at = Some(std::time::Instant::now());
                        eprintln!("🤝 peer connected — minting block 1 in {}ms (mesh-graft grace)", grace_ms);
                    } else if !producing
                        && first_peer_at.map(|t| t.elapsed().as_millis() as u64 >= grace_ms).unwrap_or(false)
                    {
                        producing = true;
                        eprintln!("🏭 grace elapsed — streaming blocks now");
                    }
                    if producing {
                    // DAGKnight: mint on the FRONTIER (settled chain + pending selected
                    // spine), not the settled chain — the settled chain advances only via
                    // the finalized drain, so all nodes converge. Linear mode mints on chain.
                    // Build the frontier FIRST: its tip (`frontier.parent_hash()`) is the
                    // block's real spine parent, which is what merge_tips must exclude.
                    let frontier_opt: Option<ChainTip> = braid.as_ref()
                        .map(|br| dag_build_frontier(&chain, br, &dag_bodies));
                    let mint_ref: &ChainTip = frontier_opt.as_ref().unwrap_or(&chain);
                    // SIGIL_DAG=1: merge parents = real DAG tips from the braid, EXCLUDING
                    // the spine parent (= the frontier tip we're building on). Excluding the
                    // settled tip instead lets the frontier tip land in BOTH parent_hash and
                    // merge_parents → braid rejects "merge parent duplicates spine parent",
                    // the block never enters recs, the spine can't deepen, nothing finalizes.
                    // (deterministic height-desc/hash-asc, capped 4 — design §3.3.)
                    let mp: Vec<BlockHash> = match braid.as_ref() {
                        Some(b) => b.merge_tips(&mint_ref.parent_hash(), 4),
                        None if dag_mode => peer_tips.iter().cloned().collect(),
                        None => Vec::new(),
                    };
                    // P1: publish the frontier miners must bind their work to.
                    // This is the parent THIS block will carry, so a solve issued
                    // now is valid for exactly this block and no other.
                    mining_bridge.publish_tip(mint_ref.height(), mint_ref.parent_hash());
                    // Gated production: mint only when a verified solve is waiting,
                    // and only if it was solved against the CURRENT frontier — a
                    // solve for a parent we've since moved off is stale work.
                    let solve: Option<sigil_api::mining::AcceptedSolve> = if mining_gated {
                        match mining_bridge.take_solve() {
                            Some(s) if s.parent_hash == mint_ref.parent_hash()
                                    && s.height == mint_ref.height() => Some(s),
                            Some(_) => continue,  // stale solve — drop, wait for work on this frontier
                            None => continue,     // no work yet — the braid idles, as PoW should
                        }
                    } else {
                        // Free-running (unchanged cadence, unchanged GHOSTDAG throughput):
                        // opportunistically credit a solve that's ALREADY queued and matches
                        // this exact frontier, but never wait for one — the braid must not
                        // start blocking on PoW just because mining is now wired. take_solve()
                        // pops destructively (no peek in MiningBridge), so a stale/mismatched
                        // solve is discarded here — same accepted tradeoff the gated path
                        // already makes on its `continue` branch, not a new loss. Anything
                        // that doesn't match falls through to the producer-wallet default.
                        mining_bridge.take_solve().filter(|s| {
                            s.parent_hash == mint_ref.parent_hash() && s.height == mint_ref.height()
                        })
                    };
                    // pull verify-once txs (already verified at mempool ingest)
                    let block_txs: Vec<SignedTx> =
                        if txgen > 0 { mempool.lock().unwrap().pull(txgen) } else { Vec::new() };
                    // ONE-CHAIN: when the adaptive emission controller is live, IT
                    // computes the reward (time-based + PID + rate) and we bake the
                    // exact amount into the coinbase; else the pure height schedule.
                    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    let reward_override: Option<u128> = emission.as_mut().map(|c| {
                        let supply = mint_ref.state_snapshot().native_supply();
                        c.calculate_block_reward(now_secs, supply)
                    });
                    match mint_next_block(mint_ref, mp, &block_txs, reward_override, solve.as_ref()) {
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
                            // SIGIL_DAG=1: capture view + body BEFORE apply moves
                            // the block, so our own blocks enter the braid (§3.3).
                            let dag_own: Option<(BlockView, crate::block::Block)> =
                                braid.is_some().then(|| (BlockView::from(&block.header), block.clone()));
                            // Settle: DAGKnight uses ONLY the finalized drain (so nodes
                            // converge); linear mode self-applies. In DAG mode our own block
                            // enters the braid like a peer's — it is NOT self-applied — and
                            // the shared chain advances solely from `drain_ordered()`.
                            let settled_ok: bool = if let Some(br) = braid.as_mut() {
                                if let Some((view, body)) = dag_own {
                                    let vh = view.hash;
                                    let _ = br.insert(view); // own block joins the DAG
                                    dag_store_body(&mut dag_bodies, dag_max_bodies, vh, body);
                                }
                                // the ONE settlement path — identical finalized order on every node
                                let (a, s, f) = dag_drain_apply(br, &mut dag_bodies, &mut chain,
                                    &mut |braw| { let _ = chain_log.append_bytes(braw); });
                                applied += a; dag_ord_skipped += s; dag_apply_failed += f;
                                true
                            } else {
                                match chain.apply(block) {
                                    Ok(_) => { let _ = chain_log.append_bytes(&bytes); true }
                                    Err(e) => { eprintln!("⚠ producer self-apply H={} failed: {}", h, e); false }
                                }
                            };
                            if settled_ok {
                                {
                                    // publish the fresh SETTLED state so the money API serves
                                    // current balances (converges across nodes via the drain).
                                    if let Some(ms) = money_state.as_ref() {
                                        if let Ok(mut w) = ms.write() { *w = chain.state_snapshot(); }
                                    }
                                    // emission watermark: producer-local reward tracking (the
                                    // SETTLED supply is chain.native_supply(), converged via the
                                    // drain — this only feeds THIS node's future reward math).
                                    if let (Some(c), Some(r)) = (emission.as_mut(), reward_override) {
                                        c.record_emission(r);
                                        c.add_block(h, now_secs, now_secs);
                                        if h % 32 == 0 { coinbase::save_controller(&snap_dir, c); }
                                    }
                                    produced += 1;
                                    // adaptive rate: retune the tick from mempool backlog every 16 blocks
                                    if let Some(g) = rate_gov.as_mut() {
                                        if produced % 16 == 0 {
                                            let backlog = mempool.lock().unwrap().len();
                                            let iv = g.update(backlog);
                                            produce_tick = tokio::time::interval_at(tokio::time::Instant::now() + iv, iv);
                                            produce_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                                        }
                                    }
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
                    //
                    // BOUNDED (2026-08-15): drain_events() returns the FULL backlog already
                    // popped from the manager's queue, and this loop is fully synchronous —
                    // it never yields back to tokio::select! until every drained event is
                    // processed. Discovered live: after any stall long enough for peers to
                    // pile up retries (a stalled/slow-starting node, or just a few peers each
                    // running their own request-ahead pipeline), this single arm can end up
                    // processing thousands of events back-to-back, some individually expensive
                    // (full-block/skeleton serves: disk read + zstd + send). While that runs,
                    // NO other select! arm — including produce_tick and the 5s heartbeat — ever
                    // gets polled, so the node looks completely dead (no new blocks, no
                    // heartbeat, /v1/mining/challenge stuck 503) even though it's actually busy
                    // and making progress. Peers are already retry-tolerant (proven live: the
                    // same peer re-requests the same range repeatedly rather than hanging), so
                    // truncating the batch and picking up the remainder on a LATER pass is a
                    // safe degradation, not data loss — and it lets production/heartbeat get a
                    // fair turn every ~250ms instead of being starved for as long as the
                    // backlog takes to fully drain (observed: 10+ minutes and still growing).
                    let mut drained = mgr.drain_events();
                    const MAX_EVENTS_PER_DRAIN: usize = 200;
                    drained.truncate(MAX_EVENTS_PER_DRAIN);
                    for ev in drained {
                        match ev {
                        flux_p2p::SwarmAppEvent::InboundRequest { peer, request_id, payload } => {
                            // Point-to-point backfill serve: answer ONE requester with
                            // the requested block range straight from our chain. No
                            // gossipsub re-broadcast — the response goes only to `peer`.
                            let req: BackfillReq = match serde_json::from_slice(&payload) {
                                Ok(r) => r,
                                Err(_) => continue,
                            };
                            // H2: authenticate the requester BEFORE serving chain data.
                            // Log-only by default (served + counted); SIGIL_HANDSHAKE_REQUIRE=1
                            // refuses. A verified session is cached until handshake expiry.
                            if let Err(refusal) = sync_auth_gate.admit(&peer.to_string(), req.handshake.as_ref(), now_ms()) {
                                eprintln!("⛔ sync-auth: refused rr-backfill [{}..={}] from {} ({:?}) · authed={} anon={} refused={}",
                                    req.from, req.to, peer, refusal,
                                    sync_auth_gate.served_authed, sync_auth_gate.served_anon, sync_auth_gate.refused);
                                continue;
                            }
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
                            // codec>=3 ('P' header / 'F' trailer) are TIP-dependent — never
                            // cache them as immutable (a stale 'P' would serve a stale anchor).
                            // codec=2 ('S') of a finalized range IS immutable (skeletons of
                            // finalized blocks never change) → cacheable like 'H'/'Z'.
                            let immutable = hi >= lo && hi < wbase && req.codec < 3;
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
                                // serve throttle removed: client request-ahead window bounds
                                // concurrent serves; finalized ranges are cache-served (memcpy).
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
                                if req.codec == 3 {
                                    // 'P' SnapshotHeader (codec=2 snapshot DISCOVERY). Tip-dependent:
                                    // base = genesis (0), anchor = our last finalized height, count =
                                    // the whole prefix. The client pages 'S' over [base..anchor].
                                    let top = chain.height().saturating_sub(1);
                                    let anchor_hash =
                                        chain.get(top).map(|b| b.header.hash()).unwrap_or([0u8; 32]);
                                    let hdr = SnapshotHeader {
                                        magic: SNAPSHOT_MAGIC,
                                        version: SNAPSHOT_VERSION,
                                        base_height: 0,
                                        anchor_height: top,
                                        anchor_hash,
                                        count: top.saturating_add(1),
                                    };
                                    let mut o = vec![b'P'];
                                    o.extend(bincode::serialize(&hdr).unwrap_or_default());
                                    eprintln!("↩ rr-backfill: served snapshot 'P' (base=0 anchor={} count={}) to {} ({} B)",
                                        top, top.saturating_add(1), peer, o.len());
                                    o
                                } else if req.codec == 2 {
                                    // 'S' skeleton page for [lo..=hi] — A's frozen 72 B/record wire
                                    // (height + block_hash + parent_hash; NO state roots, NO proofs).
                                    let recs: Vec<SkeletonRecord> =
                                        blocks.iter().map(|b| SkeletonRecord::from_header(&b.header)).collect();
                                    let mut o = vec![b'S'];
                                    o.extend(bincode::serialize(&recs).unwrap_or_default());
                                    eprintln!("↩ rr-backfill: served {} SKELETONS 'S' [{}..={}] to {} ({} B)",
                                        recs.len(), lo, hi, peer, o.len());
                                    o
                                } else if req.codec == 4 {
                                    // M1 fold-trailer: archive_root = BLAKE3 over bincode of each SkeletonRecord in order
                                    // (matches the client SnapshotVerifier::push). anchor_sig/fold_blob empty = the
                                    // structural pull the client finalize accepts on root-match; M2 adds SQIsign+flux_fold.
                                    let recs: Vec<SkeletonRecord> = blocks.iter().map(|b| SkeletonRecord::from_header(&b.header)).collect();
                                    let mut hh = blake3::Hasher::new();
                                    { let _ga = chain.height().saturating_sub(1); let _gw = chain.window_base(); let mut fb: Vec<crate::block::Block> = Vec::new(); if _gw > 0 { fb.extend(chain_log.get_range(0, _gw.saturating_sub(1))); } for h2 in _gw..=_ga { if let Some(b2) = chain.get(h2) { fb.push(b2.clone()); } } for b2 in &fb { let r2 = SkeletonRecord::from_header(&b2.header); hh.update(&bincode::serialize(&r2).unwrap_or_default()); } }
                                    let trailer = sigil_header::SnapshotTrailer { archive_root: *hh.finalize().as_bytes(), anchor_sig: Vec::new(), fold_blob: Vec::new() };
                                    let mut o = vec![b'F'];
                                    o.extend(bincode::serialize(&trailer).unwrap_or_default());
                                    eprintln!("served 'F' trailer M1 (root) [{}..={}] to {} ({} B)", lo, hi, peer, o.len());
                                    o
                                } else {
                                    // codec 0/1: monitor path — bincode Vec<header>, ~20× smaller, no JSON.
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
                                }
                            } else {
                                let resp = BackfillResp {
                                    blocks,
                                };
                                eprintln!("↩ rr-backfill: served {} blocks [{}..={}] to {}",
                                    resp.blocks.len(), lo, hi, peer);
                                bincode::serialize(&resp).unwrap_or_default()
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
                                if let Some(br) = braid.as_mut() {
                                    // SIGIL_DAG=1 REAL path (design §3.2): precheck →
                                    // braid.insert → park+backfill on missing parents →
                                    // drain the finalized order and state-apply exactly
                                    // the spine blocks that extend the local tip through
                                    // the UNMODIFIED ChainTip::apply chokepoint.
                                    tx_total += block.header.tx_count as u64;
                                    if let Err(e) = block.header.precheck() {
                                        eprintln!("⚠ braid: precheck reject from {}: {}", from, e);
                                        continue;
                                    }
                                    let bhash = block.hash();
                                    let bheight = block.header.height;
                                    if bheight > net_tip { net_tip = bheight; }
                                    match br.insert(BlockView::from(&block.header)) {
                                        InsertOutcome::Inserted { .. } => {
                                            dag_store_body(&mut dag_bodies, dag_max_bodies, bhash, block);
                                            // legacy tips deque stays fed for one release
                                            // (harmless; unused when braid is Some).
                                            peer_tips.push_back(bhash);
                                            while peer_tips.len() > 4 { peer_tips.pop_front(); }
                                        }
                                        InsertOutcome::MissingParents(_missing) => {
                                            // Park the body for the braid AND buffer it for the
                                            // LINEAR contiguous applier (catch-up is linear; the
                                            // braid re-anchors at the frontier via self-heal).
                                            // Pull the ancestry gap with the EXISTING throttled
                                            // rr-backfill shape — responses land in bf_rx.
                                            pending_insert(&mut pending, chain.height(), bheight, block.clone());
                                            dag_store_body(&mut dag_bodies, dag_max_bodies, bhash, block);
                                            if last_req.elapsed() >= std::time::Duration::from_millis(15) {
                                                last_req = std::time::Instant::now();
                                                if let Some(peer) = mgr.connected_peers().into_iter().next() {
                                                    let req = BackfillReq {
                                                        from: chain.height(),
                                                        to: bheight.saturating_add(1),
                                                        headers_only: false,
                                                        codec: 0,
                                                        handshake: Some((*sync_hs).clone()),
                                                    };
                                                    eprintln!("⇪ rr-backfill(braid): missing parents at H={} — requesting [{}..={}] from {}",
                                                        bheight, req.from, req.to, peer);
                                                    let mgr2 = std::sync::Arc::clone(&mgr);
                                                    let bf_tx2 = bf_tx.clone();
                                                    tokio::spawn(async move {
                                                        if let Ok(payload) = serde_json::to_vec(&req) {
                                                            if let Ok(bytes) = mgr2.send_request(peer, payload).await {
                                                                if let Ok(resp) = bincode::deserialize::<BackfillResp>(&bytes) {
                                                                    let _ = bf_tx2.send(resp.blocks).await;
                                                                }
                                                            }
                                                        }
                                                    });
                                                }
                                            }
                                            continue;
                                        }
                                        InsertOutcome::Duplicate => { continue; }
                                        InsertOutcome::BelowFinal { .. } => {
                                            dag_below_final += 1; // reorg-window guard held
                                            continue;
                                        }
                                        InsertOutcome::Rejected(r) => {
                                            dag_rejected += 1;
                                            if dag_rejected % 1000 == 1 {
                                                eprintln!("⚠ braid: reject from {} at H={}: {} ({} total)", from, bheight, r, dag_rejected);
                                            }
                                            // Wedge self-heal: sustained rejects (e.g. pending
                                            // overflow after a deep late-join) mean the braid
                                            // lost the frontier — rebuild it base-anchored from
                                            // the CURRENT window. Progress-gated: only after the
                                            // linear catch-up has actually advanced the tip.
                                            if dag_rejected.saturating_sub(dag_last_reseed_rejects) >= 5000
                                                && chain.height() > dag_last_reseed_height
                                            {
                                                dag_last_reseed_rejects = dag_rejected;
                                                dag_last_reseed_height = chain.height();
                                                eprintln!("🕸 braid re-anchor after catch-up progress ({} rejects, tip H={})",
                                                    dag_rejected, chain.height());
                                                *br = dag_seed_braid(&chain);
                                            }
                                            continue;
                                        }
                                    }
                                    let (a, s, f) = dag_drain_apply(
                                        br, &mut dag_bodies, &mut chain,
                                        &mut |braw| { let _ = chain_log.append_bytes(braw); });
                                    applied += a;
                                    dag_ord_skipped += s;
                                    dag_apply_failed += f;
                                    if received % 200 == 0 {
                                        let secs = t_start.elapsed().as_secs_f64().max(1e-6);
                                        let st = br.stats();
                                        let oh = br.order_hash();
                                        eprintln!("🕸 braid: recv {} ({:.1}/s) · applied {} · ord-skipped {} · below-final {} · rejected {} · apply-fail {} · window {} pending {} final H={} tip H={} · order_hash {} · {} txs ({:.0} TPS)",
                                            received, received as f64 / secs, applied,
                                            dag_ord_skipped, dag_below_final, dag_rejected, dag_apply_failed,
                                            st.window, st.pending, st.finalized_height, chain.height(),
                                            hex::encode(&oh[..8]),
                                            tx_total, tx_total as f64 / secs);
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
                                    if h > net_tip { net_tip = h; }
                                    pending_insert(&mut pending, chain.height(), h, block);
                                    if last_req.elapsed() >= std::time::Duration::from_millis(15) {
                                        last_req = std::time::Instant::now();
                                        if let Some(peer) = mgr.connected_peers().into_iter().next() {
                                            let req = BackfillReq { from: expected, to: expected.saturating_add(8192), headers_only: false, codec: 0, handshake: Some((*sync_hs).clone()) };
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
                                                    if let Ok(resp) = bincode::deserialize::<BackfillResp>(&bytes) {
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
                                if peer_h > net_tip { net_tip = peer_h; }
                                if peer_h > expected
                                    && last_req.elapsed() >= std::time::Duration::from_millis(15)
                                {
                                    last_req = std::time::Instant::now();
                                    if let Some(peer) = mgr.connected_peers().into_iter().next() {
                                        let req = BackfillReq {
                                            from: expected,
                                            to: expected.saturating_add(8192),
                                            headers_only: false,
                                            codec: 0, // node-to-node needs full blocks; raw JSON path
                                            handshake: Some((*sync_hs).clone()),
                                        };
                                        eprintln!("⇪ rr-backfill: behind via peer-heights (have {}, net {}) — requesting [{}..={}]",
                                            expected, peer_h, req.from, req.to);
                                        let mgr2 = std::sync::Arc::clone(&mgr);
                                        let bf_tx2 = bf_tx.clone();
                                        tokio::spawn(async move {
                                            if let Ok(payload) = serde_json::to_vec(&req) {
                                                if let Ok(bytes) = mgr2.send_request(peer, payload).await {
                                                    if let Ok(resp) = bincode::deserialize::<BackfillResp>(&bytes) {
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
                        // Named peer identity on connect/disconnect — previously only the
                        // aggregate `peer_count` was ever logged, so there was no way to
                        // tell a real distinct mesh peer apart from a stale/duplicate count
                        // without reading someone else's logs. addr lets an operator map a
                        // peer_id to a known box (e.g. an IP matching Gamma/Beta) at a glance.
                        flux_p2p::SwarmAppEvent::PeerConnected { peer_id, addr } => {
                            eprintln!("🔗 peer connected  {peer_id}  {addr}");
                        }
                        flux_p2p::SwarmAppEvent::PeerDisconnected { peer_id } => {
                            eprintln!("🔌 peer disconnected  {peer_id}");
                        }
                        _ => {}
                        }
                    }
                }
                Some(vals) = bf_rx.recv() => {
                    // Point-to-point backfill response arrived: buffer each block by
                    // height, then drain `pending` contiguously into the chain via the
                    // same apply path the live-block branch uses.
                    if let Some(br) = braid.as_mut() {
                        // SIGIL_DAG=1 catch-up is LINEAR, exactly like the proven
                        // non-dag arm: buffer by height, contiguously apply through
                        // the UNMODIFIED ChainTip::apply chokepoint. The braid is a
                        // FRONTIER structure — it is fed opportunistically as blocks
                        // apply, and the wedge self-heal re-anchors it at the new
                        // window once catch-up makes progress. Pushing a deep gap
                        // through the braid (pending cap 4096) is what wedged the
                        // first braid-c smoke test — never again.
                        for block in vals {
                            if block.header.precheck().is_err() { continue; }
                            let h = block.header.height;
                            pending_insert(&mut pending, chain.height(), h, block);
                        }
                        while let Some(b) = pending.remove(&chain.height()) {
                            let bhash = b.hash();
                            let view = BlockView::from(&b.header);
                            let braw = serde_json::to_vec(&b).unwrap_or_default();
                            match chain.apply(b.clone()) {
                                Ok(_) => {
                                    let _ = chain_log.append_bytes(&braw);
                                    applied += 1;
                                    backfilled += 1;
                                    if matches!(br.insert(view), InsertOutcome::Inserted { .. }) {
                                        dag_store_body(&mut dag_bodies, dag_max_bodies, bhash, b);
                                    }
                                }
                                Err(_) => { dag_apply_failed += 1; break; }
                            }
                        }
                        // Wedge self-heal: after sustained rejects AND real catch-up
                        // progress, rebuild the braid base-anchored at the current
                        // window so it re-acquires the frontier. Progress-gated so a
                        // stalled node cannot churn reseeds.
                        if dag_rejected.saturating_sub(dag_last_reseed_rejects) >= 5000
                            && chain.height() > dag_last_reseed_height
                        {
                            dag_last_reseed_rejects = dag_rejected;
                            dag_last_reseed_height = chain.height();
                            eprintln!("🕸 braid re-anchor after catch-up progress ({} rejects, tip H={})",
                                dag_rejected, chain.height());
                            *br = dag_seed_braid(&chain);
                        }
                        let (a, s, f) = dag_drain_apply(
                            br, &mut dag_bodies, &mut chain,
                            &mut |braw| { let _ = chain_log.append_bytes(braw); });
                        applied += a;
                        backfilled += a;
                        dag_ord_skipped += s;
                        dag_apply_failed += f;
                    } else if !diverged {
                        for v in vals {
                            if let Ok(block) = Ok::<crate::block::Block, ()>(v) {
                                let h = block.header.height;
                                pending_insert(&mut pending, chain.height(), h, block);
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

/// Build (or REBUILD — the wedge self-heal) a braid seeded from the local
/// chain's in-RAM window. A pruned window (base > 0) anchors the braid at the
/// oldest in-RAM block via `Braid::new_with_base` — trusted because this node
/// applied it through the state chokepoint — so seeds chain cleanly instead
/// of parking against unknown pre-window ancestry.
fn dag_seed_braid(chain: &ChainTip) -> Braid {
    let cfg = BraidConfig::from_env();
    eprintln!("🕸 braid config: final_depth={} max_window={} max_pending={} max_merge_parents={} ghostdag_k={}",
        cfg.final_depth, cfg.max_window, cfg.max_pending, cfg.max_merge_parents,
        cfg.ghostdag_k.map(|k| k.to_string()).unwrap_or_else(|| "off (v1)".to_string()));
    let base_h = chain.window_base();
    let (mut b, seed_from) = if base_h > 0 {
        match chain.get(base_h) {
            Some(base_blk) => {
                let bv = BlockView::from(&base_blk.header);
                eprintln!("🕸 braid base-anchored at H={} (pruned window; pre-window ancestry trusted-local)", base_h);
                (Braid::new_with_base(cfg, bv.hash, bv.height), base_h + 1)
            }
            None => (Braid::new(cfg), base_h),
        }
    } else {
        (Braid::new(cfg), 0)
    };
    let mut seeded = 0usize;
    for hh in seed_from..chain.height() {
        if let Some(blk) = chain.get(hh) {
            let view = BlockView::from(&blk.header);
            // Window blocks may carry merge edges to bodies this node never
            // stored (the other strand, pre-catch-up). Those references are
            // committed inside headers we applied through the chokepoint —
            // anchor them as trusted history so the seed chains instead of
            // cascading into the parked set.
            for mp in &view.merge_parents {
                b.anchor_trusted(*mp, view.height.saturating_sub(1));
            }
            if matches!(b.insert(view), InsertOutcome::Inserted { .. }) {
                seeded += 1;
            }
        }
    }
    eprintln!("🕸 braid seeded with {} local window blocks", seeded);
    b
}

/// SIGIL_DAG=1 drain step (design §3.2 step 4): pull the braid's newly
/// finalized order and state-apply EXACTLY the blocks that extend the local
/// tip (`parent_hash == tip && height == next`) through the full, UNMODIFIED
/// `ChainTip::apply` chokepoint (precheck → commit_state_transition →
/// check_roots_match). Ordered blocks that are off-spine, non-extending, or
/// already applied (the producer self-applies its own) are counted in
/// `skipped` and NOT state-applied — v0 semantics per
/// `docs/SIGIL_DAGKNIGHT_LANE_v0.md` §3.4. Applied blocks are handed to
/// `persist` (the chain_log append path) exactly as the linear path does.
/// Bodies below the braid's finalized height are evicted after the drain.
/// Returns `(applied, skipped, failed)`.
/// DAGKnight: build the speculative *frontier* the producer mints on — a clone of
/// the settled chain with the braid's pending selected-spine suffix applied on top.
/// The settled chain advances ONLY via the finalized `drain_ordered()` order (so every
/// node converges); but a producer must build on a tip *ahead* of finality, and its
/// coinbase roots must match what the drain will recompute — which they do, because the
/// frontier applies the same selected-spine blocks the drain will. Rebuilt each tick →
/// reorg-safe. Blocks below the settled tip won't extend the frontier and are skipped.
fn dag_build_frontier(
    chain: &ChainTip,
    braid: &Braid,
    dag_bodies: &std::collections::HashMap<BlockHash, crate::block::Block>,
) -> ChainTip {
    let mut frontier = chain.clone();
    // Follow the braid's SELECTED SPINE — the `parent_hash` walk back from
    // `selected_tip()` (max-height, min-hash). A greedy "first block that fits"
    // walk fails to deepen: with two producers, height-N siblings are built on
    // DIFFERENT height-(N-1) siblings, so a random height-1 pick has no matching
    // height-2 child → the frontier caps one above the settled tip and every tick
    // re-mints the same height forever (the DAG stays a flat bush, never clears
    // final_depth, nothing finalizes). Walking the ONE selected spine gives a
    // connected parent→child path, so the frontier reaches the selected tip and
    // minting extends it by one → the spine deepens → finality advances.
    let dbg = std::env::var("SIGIL_FRONTIER_DEBUG").ok().as_deref() == Some("1");
    let Some(tip) = braid.selected_tip() else {
        if dbg { let s = braid.stats(); eprintln!("🔬 frontier: selected_tip=None base={} window={} pending={} tips={} rejected={} dropped={}", frontier.height(), s.window, s.pending, s.tips, s.rejected, s.dropped); }
        return frontier;
    };
    let tip_h = dag_bodies.get(&tip).map(|b| b.header.height as i64).unwrap_or(-1);
    // Collect the spine bodies from the selected tip back down to (not including)
    // the settled tip's height, then apply in ascending height order.
    let base = frontier.height(); // first height the frontier still needs
    let mut path: Vec<BlockHash> = Vec::new();
    let mut cur = tip;
    loop {
        let Some(b) = dag_bodies.get(&cur) else { break };
        if b.header.height < base { break; } // reached the settled region
        path.push(cur);
        cur = b.header.parent_hash;
    }
    let path_len = path.len();
    path.reverse(); // ascending height, spine-connected
    let mut applied_n = 0usize;
    let mut fail_reason = "";
    for oh in path {
        let Some(b) = dag_bodies.get(&oh) else { fail_reason = "body-missing"; break };
        if b.header.parent_hash == frontier.parent_hash() && b.header.height == frontier.height() {
            if let Err(e) = frontier.apply(b.clone()) {
                fail_reason = "apply-err";
                if dbg { eprintln!("🔬 frontier apply-err at h={}: {}", b.header.height, e); }
                break;
            }
            applied_n += 1;
        } else {
            fail_reason = "no-chain";
            break; // spine block doesn't chain onto the frontier — stop cleanly
        }
    }
    if dbg {
        let s = braid.stats();
        eprintln!("🔬 frontier: tip_h={} base={} path_len={} applied={} fail={} → frontier_h={} | window={} pending={} tips={} emitted={} fin_h={} rej={} drop={}",
            tip_h, base, path_len, applied_n, fail_reason, frontier.height(),
            s.window, s.pending, s.tips, s.emitted_total, s.finalized_height, s.rejected, s.dropped);
    }
    frontier
}

fn dag_drain_apply(
    braid: &mut Braid,
    dag_bodies: &mut std::collections::HashMap<BlockHash, crate::block::Block>,
    chain: &mut ChainTip,
    persist: &mut dyn FnMut(&[u8]),
) -> (u64, u64, u64) {
    // Cross-node divergence detector (opt-in, test meshes): log every
    // finalized emission with its sequence index. Two nodes' ⛓ streams must
    // be prefix-identical — the wire analogue of sim gate S1.
    let order_log = std::env::var("SIGIL_DAG_ORDER_LOG").ok().as_deref() == Some("1");
    let (mut applied, mut skipped, mut failed) = (0u64, 0u64, 0u64);
    // Advance the braid's frozen order + slide its retention window (side
    // effects: emit/cleanup). The returned Kahn batch is NOT what we settle on
    // — it interleaves sibling forks, and picking "first block that extends"
    // out of it follows a DIFFERENT fork than `dag_build_frontier` (which walks
    // `selected_tip`). When those two disagree at a fork the settled chain and
    // the frontier stall on opposite branches. So settlement follows the SAME
    // canonical spine as the frontier: the `selected_tip` parent-walk, applied
    // only up to `finalized_height`. Deterministic per DAG ⇒ every node lands
    // the identical settled chain.
    let _ = braid.drain_ordered();
    let fin = braid.finalized_height();
    let base = chain.height();
    // Walk the selected spine from the tip down, keeping the finalized band
    // [base, fin]; then apply ascending so each block extends the settled tip.
    let mut path: Vec<BlockHash> = Vec::new();
    if let Some(tip) = braid.selected_tip() {
        let mut cur = tip;
        loop {
            let Some(b) = dag_bodies.get(&cur) else { break };
            let h = b.header.height;
            if h < base { break; } // reached the already-settled region
            if h <= fin { path.push(cur); } // only the finalized prefix settles
            cur = b.header.parent_hash;
        }
    }
    path.reverse();
    let mut ord_idx = chain.height();
    for oh in path {
        let Some(body) = dag_bodies.get(&oh) else { skipped += 1; continue };
        if body.header.parent_hash != chain.parent_hash() || body.header.height != chain.height() {
            skipped += 1; // not spine-contiguous with the settled tip (transient)
            continue;
        }
        if order_log {
            eprintln!("⛓ ord #{} {}", ord_idx, hex::encode(&oh[..12]));
        }
        ord_idx += 1;
        let braw = serde_json::to_vec(body).unwrap_or_default();
        match chain.apply(body.clone()) {
            Ok(()) => {
                persist(&braw);
                applied += 1;
            }
            Err(e) => {
                failed += 1;
                eprintln!("🔴 braid spine apply FAILED at H={} — {}", body.header.height, e);
            }
        }
    }
    // Retain bodies from the settled tip upward (the frontier needs the pending
    // spine suffix); anything below the new settled height is spent.
    let keep = chain.height().saturating_sub(1);
    dag_bodies.retain(|_, b| b.header.height >= keep);
    (applied, skipped, failed)
}

/// Bounded insert into the SIGIL_DAG=1 body store. On overflow the
/// lowest-height resident body is dropped first (approximation of the design's
/// "lowest-height off-spine first" — the linear path's 200k `pending` cap is
/// the precedent). O(n) scan only on overflow.
/// Hard cap on the height-keyed `pending` block buffer, and how far ahead of the
/// local tip a gossiped block may be buffered at all.
///
/// **Why this exists (measured, 2026-08-01).** `pending` is a
/// `BTreeMap<u64, Block>` fed from FOUR sites. Only the legacy linear path was
/// capped (`if pending.len() < 200_000`); the **braid gossip path** and both
/// backfill-response paths inserted with no bound beyond `h >= chain.height()`.
/// The live node runs `SIGIL_DAG=1`, i.e. the uncapped path.
///
/// A `sigil-memwedge` chronos run measured the true cost at **2,803 bytes per
/// buffered height** (header only — a full Block is larger). So a node that
/// falls behind buffers the whole gap: 1M heights ≈ 2.6 GiB, 10M ≈ 26 GiB,
/// against a `MemoryHigh` of 6 GiB. Crossing that line puts the process into
/// kernel direct-reclaim throttling (measured on the wedged node: PSI memory
/// `full avg300 = 97.17`, 1.93M throttle events, `pgscan_kswapd = 0` so ALL
/// reclaim is synchronous) — which makes it apply blocks *slower*, which makes
/// it fall *further* behind, which buffers *more*. That is the death spiral
/// that wedged sigil-node three times.
///
/// The distance bound matters as much as the count bound: 200k heights of slack
/// is still ~560 MiB. Blocks beyond the horizon are dropped, not buffered — the
/// backfill requester will re-fetch them in order as the tip advances, which is
/// the mechanism that is supposed to close a gap anyway.
const PENDING_MAX_ENTRIES: usize = 32_768;
const PENDING_MAX_AHEAD: u64 = 32_768;

/// Buffer a gossiped/backfilled block by height, bounded in BOTH directions.
/// Returns true if it was retained. Every `pending` insert must go through here.
fn pending_insert(
    pending: &mut std::collections::BTreeMap<u64, crate::block::Block>,
    tip_height: u64,
    height: u64,
    block: crate::block::Block,
) -> bool {
    // Already settled — never buffer the past.
    if height < tip_height {
        return false;
    }
    // Beyond the horizon: dropping is correct. Ordered backfill will supply it.
    if height.saturating_sub(tip_height) > PENDING_MAX_AHEAD {
        return false;
    }
    if pending.len() >= PENDING_MAX_ENTRIES && !pending.contains_key(&height) {
        // Full: keep the CLOSEST-to-tip work (that is what unblocks the applier)
        // and evict the farthest-ahead entry, but only if the newcomer is nearer.
        let farthest = match pending.keys().next_back().copied() {
            Some(k) => k,
            None => return false,
        };
        if height >= farthest {
            return false;
        }
        pending.remove(&farthest);
    }
    pending.entry(height).or_insert(block);
    true
}

fn dag_store_body(
    dag_bodies: &mut std::collections::HashMap<BlockHash, crate::block::Block>,
    cap: usize,
    hash: BlockHash,
    block: crate::block::Block,
) {
    if dag_bodies.len() >= cap && !dag_bodies.contains_key(&hash) {
        if let Some(evict) = dag_bodies
            .iter()
            .min_by_key(|(h, b)| (b.header.height, **h))
            .map(|(h, _)| *h)
        {
            dag_bodies.remove(&evict);
        }
    }
    dag_bodies.insert(hash, block);
}

/// Mint an EMPTY block at the current tip — a no-tx block that just advances
/// the chain. Used by the `start` producer loop to stream blocks across the
/// network for cross-host throughput measurement. The transition is empty so
/// the four roots are unchanged from the parent; the receiver re-applies the
/// empty transition and the roots match. Crypto fields zeroed (Phase 0).
fn mint_next_block(
    chain: &ChainTip,
    merge_parents: Vec<BlockHash>,
    txs: &[SignedTx],
    reward_override: Option<u128>,
    solve: Option<&sigil_api::mining::AcceptedSolve>,
) -> Result<Block> {
    let height = chain.height();
    let parent = chain.parent_hash();
    // P1 mining-onto-the-braid: when this block is minted for a verified
    // dual-lane solve, the 292-byte nonce field carries the winning
    // (nonce ‖ blake4_hash) — the same carrier layout the ledger header uses —
    // and the real Wesolowski proof rides in `vdf_proof`. A follower rebuilds
    // the challenge from (parent_hash, height, producer) and re-verifies BOTH
    // lanes: `sigil_api::mining::verify_header_pow`. Without a solve the fields
    // stay zeroed (the free-running dyno, unchanged).
    let nonce = match solve {
        Some(s) => SqiSignature::from_array(sigil_api::mining::pack_nonce_carrier(s.nonce, s.blake4_hash)),
        None => SqiSignature::from_array([0u8; SQISIGN_L5_LEN]),
    };
    // vdf_input MUST satisfy header.precheck: BLAKE3(parent || nonce.0).
    let mut h = blake3::Hasher::new();
    h.update(&parent);
    h.update(nonce.as_bytes());
    let vdf_input = *h.finalize().as_bytes();

    // ONE-CHAIN step 1: the braid mints a REAL coinbase — the producer credits
    // itself block_reward(height) through the shared money chokepoint, so money
    // now lives IN the graph (not a separate rpcd chain). The reward mutation is
    // in the block body, so every follower re-applies it and the root-match check
    // in ChainTip::apply passes. SIGIL_COINBASE=0 restores the empty-block dyno.
    // ONE-CHAIN: the full block body = coinbase + user sends, applied in order
    // against the evolving state. reward: SIGIL_COINBASE=0 → no coinbase; the
    // adaptive controller (if live) supplies the exact amount; else the height
    // schedule. Sends flow through apply_tx → real balance moves on the braid.
    let coinbase_on = std::env::var("SIGIL_COINBASE").map(|v| v != "0").unwrap_or(true);
    let state = chain.state_snapshot();
    let reward = if coinbase_on { reward_override } else { Some(0u128) };
    // Mined block → the reward is split over the verified solves for this
    // height (pool-share economics: dev-fee + commons + proportional payout,
    // winner absorbs the remainder); otherwise the node's configured producer
    // wallet takes the whole thing (a one-entry "shares" map, which is exactly
    // what the split degenerates to — no behavior change from before this was
    // wired unless a master wallet is genesis-committed on this chain).
    let (winner, shares): (WalletId, std::collections::HashMap<WalletId, u64>) = match solve {
        Some(s) => (s.wallet, s.shares.clone()),
        None => {
            let w = coinbase::producer_wallet();
            (w, std::collections::HashMap::from([(w, 1u64)]))
        }
    };
    let (transition, roots, block_events, included_txs) =
        coinbase::build_block_body_for_shares(&state, height, reward, txs, winner, &shares);
    // Commit the verify-once txs: a sequential BLAKE3 root over their intent
    // hashes + the count. The signatures were verified ONCE at mempool ingest;
    // the producer-sig over this header binds the producer to this exact set.
    let txs_root = {
        let mut th = blake3::Hasher::new();
        for t in &included_txs { th.update(&t.tx.hash()); }
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
        vdf_proof: match solve {
            Some(s) => WesolowskiProof { y: s.vdf.y.clone(), pi: s.vdf.pi.clone(), t: s.vdf.t },
            None => WesolowskiProof { y: vec![], pi: vec![], t: 0 },
        },
        difficulty: solve.map(|s| s.bits as u64).unwrap_or(0),
        wallet_state_root: roots.wallet_state_root,
        dex_state_root: roots.dex_state_root,
        event_log_root: roots.event_log_root,
        contract_state_root: roots.contract_state_root,
        state_transition_proof: StarkProof { bytes: vec![], public_inputs_hash: [0u8; 32] },
        txs_merkle_root: txs_root,
        tx_count: included_txs.len() as u32,
        fluxc_artifact_proof: ProofBundle {
            artifact_blake3: [0u8; 32],
            sqisign_sig: vec![],
            sqisign_pubkey: vec![],
            settle_tx: None,
        },
        sig_scheme: SigScheme::SqiSign5,
        // The miner's wallet — the work is bound to it (the header the miner
        // hashed commits to this exact wallet), so it can't be re-pointed.
        producer: solve.map(|s| s.wallet).unwrap_or([0u8; 32]),
        producer_sig: SignatureBytes(vec![0u8; SQISIGN_L5_LEN]),
    };
    Ok(Block { header, transition, events: block_events })
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

    /// END-TO-END: a real `mint_next_block()` call with a verified multi-wallet
    /// solve actually credits the pool-share split, not just the isolated
    /// `coinbase::split_coinbase_mutations` unit — proves the WIRING (the part
    /// unique to this integration, since the math itself is unit-tested in
    /// coinbase.rs) is correct: solve.shares reaches the minted block's coinbase,
    /// and every credited wallet's balance is visible after `chain.apply()`.
    #[test]
    fn mint_next_block_credits_a_real_pool_share_solve() {
        let mut chain = ChainTip::new();
        chain.apply(build_genesis().unwrap()).expect("genesis applies");
        let winner: WalletId = [0x11; 32];
        let other: WalletId = [0x22; 32];
        let shares = std::collections::HashMap::from([(winner, 3u64), (other, 1u64)]);
        let solve = sigil_api::mining::AcceptedSolve {
            wallet: winner,
            height: chain.height(),
            parent_hash: chain.parent_hash(),
            nonce: 0,
            blake4_hash: 0,
            vdf: flux_vdf::VdfProof { y: vec![], pi: vec![], t: 0 },
            bits: 16,
            shares,
        };
        let reward = sigil_emission::block_reward(chain.height());
        let block = mint_next_block(&chain, Vec::new(), &[], None, Some(&solve))
            .expect("mint with a solve must succeed");
        chain.apply(block).expect("minted block applies cleanly");

        let state = chain.state_snapshot();
        let winner_bal = state.balance_of(&winner, &sigil_state::NATIVE);
        let other_bal = state.balance_of(&other, &sigil_state::NATIVE);
        let master_bal = state.balance_of(&MASTER_WALLET_GENESIS, &sigil_state::NATIVE);
        let commons_bal = state.balance_of(&sigil_bank::COMMONS_WALLET, &sigil_state::NATIVE);
        assert!(other_bal > 0, "the minority contributor must be credited");
        assert!(winner_bal > other_bal, "winner's 3x weight must pay more than other's 1x");
        assert!(master_bal > 0, "genesis already commits a master wallet — the dev fee must apply");
        assert!(commons_bal > 0, "the commons tithe must apply");
        assert_eq!(
            winner_bal + other_bal + master_bal + commons_bal,
            reward,
            "every unit of the block reward must land in exactly one of these four wallets"
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

    /// End-to-end RAM-window pruning: applying past `chain::WINDOW` blocks must
    /// drop the oldest from RAM (advancing `window_base`) WITHOUT corrupting
    /// `height()` — the memory bound that lets the chain grow without OOM while
    /// the tip stays accurate. Old heights vanish from RAM (served from the
    /// on-disk chain-log instead); the tip stays resident.
    #[test]
    fn window_prunes_old_blocks_but_height_stays_correct() {
        let mut chain = ChainTip::new();
        chain.apply(build_genesis().unwrap()).unwrap(); // genesis at height 0
        let extra = crate::chain::WINDOW as u64 + 5; // overflow the window
        for h in 1..=extra {
            let mut staging = chain.state_snapshot();
            let t = sigil_state::StateTransition { at_height: h, mutations: vec![] };
            let roots = sigil_state::commit_state_transition(&mut staging, &t, h).unwrap();
            let block = build_block_at(h, chain.parent_hash(), roots, t, vec![]).unwrap();
            chain.apply(block).unwrap();
        }
        let total = extra + 1; // genesis + extra blocks
        assert_eq!(chain.height(), total, "height must still count pruned blocks");
        assert!(chain.window_base() > 0, "oldest blocks must have been pruned from RAM");
        assert_eq!(chain.window_base(), total - crate::chain::WINDOW as u64, "window holds exactly WINDOW blocks");
        assert!(chain.get(0).is_none(), "pruned genesis is gone from RAM");
        let tip_height = chain.height() - 1;
        assert!(chain.get(tip_height).is_some(), "the tip block stays resident in RAM");
    }
}

#[cfg(test)]
mod dag_wiring_tests {
    //! S5b-lite (design §4): the SIGIL_DAG=1 spine-apply eligibility logic —
    //! three consensus-valid blocks driven through a REAL `Braid` and the REAL
    //! `ChainTip::apply` chokepoint, delivered in worst-case (children-first)
    //! order. No node boot, no networking, no new dev-deps.
    use super::*;

    #[test]
    fn braid_drain_applies_spine_blocks_in_order() {
        // Build genesis + 2 successors on a scratch chain (the producer path).
        let mut builder = ChainTip::new();
        let g = build_genesis().expect("genesis");
        let mut blocks = vec![g.clone()];
        builder.apply(g).expect("apply genesis");
        for _ in 0..2 {
            let b = mint_next_block(&builder, vec![], &[], None, None).expect("mint");
            blocks.push(b.clone());
            builder.apply(b).expect("apply");
        }

        // Follower: fresh chain + braid with final_depth 0 so every ordered
        // block finalizes at the tip (the wiring under test, not finality lag).
        let mut chain = ChainTip::new();
        let mut braid = Braid::new(BraidConfig { final_depth: 0, ..BraidConfig::default() });
        let mut dag_bodies = std::collections::HashMap::new();
        let mut persisted: Vec<Vec<u8>> = Vec::new();
        let (mut applied, mut skipped) = (0u64, 0u64);
        // Worst-case arrival: children before parents (park → cascade unpark).
        for b in blocks.iter().rev() {
            let outcome = braid.insert(BlockView::from(&b.header));
            assert!(
                !matches!(outcome, InsertOutcome::Rejected(_)),
                "unexpected reject: {outcome:?}"
            );
            dag_store_body(&mut dag_bodies, 32, b.hash(), b.clone());
            let (a, s, f) = dag_drain_apply(&mut braid, &mut dag_bodies, &mut chain, &mut |braw| {
                persisted.push(braw.to_vec());
            });
            applied += a;
            skipped += s;
            assert_eq!(f, 0, "no apply failures through the real chokepoint");
        }

        // All three landed IN SPINE ORDER through the unmodified apply path.
        assert_eq!(applied, 3);
        assert_eq!(skipped, 0, "every ordered block extended the tip");
        assert_eq!(chain.height(), 3);
        assert_eq!(chain.parent_hash(), blocks[2].hash(), "tip = last minted block");
        assert_eq!(persisted.len(), 3, "each applied block hit the persist hook");
        let persisted_heights: Vec<u64> = persisted
            .iter()
            .map(|raw| serde_json::from_slice::<crate::block::Block>(raw).unwrap().header.height)
            .collect();
        assert_eq!(persisted_heights, vec![0, 1, 2], "persisted in linearized order");

        // Re-feeding an old block is refused (Duplicate or BelowFinal) and the
        // chain is untouched — the reorg-window guard end-to-end.
        let refeed = braid.insert(BlockView::from(&blocks[1].header));
        assert!(
            matches!(refeed, InsertOutcome::Duplicate | InsertOutcome::BelowFinal { .. }),
            "old block must be refused: {refeed:?}"
        );
        assert_eq!(chain.height(), 3);
    }
}
